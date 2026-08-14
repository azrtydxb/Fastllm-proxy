//! The published spec against the router that actually exists.
//!
//! # Why this is a test and not a generator
//!
//! `openapi.json` is checked in and served by `GET /openapi.json`. Anything
//! checked in drifts: a route gets added, the spec does not, and the
//! difference is invisible until somebody generates a client from it and
//! wonders why a call 404s.
//!
//! Deriving the spec at runtime from the router would trade that for a
//! different problem — every handler's parameters and responses would have to
//! be reproduced in a second form anyway, so the drift would just move.
//!
//! So the file stays authored and this compares it against the source. A
//! route added without a spec entry fails the build; a spec entry for a route
//! that no longer exists fails too, which is the direction people forget.

use std::collections::{BTreeMap, BTreeSet};

fn source(file: &str) -> String {
    std::fs::read_to_string(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), file))
        .unwrap_or_else(|e| panic!("reading {file}: {e}"))
}

/// Every `(path, method)` the admin router registers.
///
/// Parsed rather than reflected because axum's `Router` does not expose its
/// routes. The parse is deliberately dumb — it matches the literal
/// `.route("...", get(...).post(...))` shape this file uses everywhere — and
/// a change to that shape fails loudly here rather than silently matching
/// nothing.
fn router_routes() -> BTreeMap<String, BTreeSet<String>> {
    let src = source("src/control/api.rs");
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for chunk in src.split(".route(").skip(1) {
        let Some(path_start) = chunk.find('"') else {
            continue;
        };
        let Some(path_len) = chunk[path_start + 1..].find('"') else {
            continue;
        };
        let path = &chunk[path_start + 1..path_start + 1 + path_len];
        if !path.starts_with('/') {
            continue;
        }
        // The handler list ends at the next `.route(` or the end of the
        // builder chain; taking a bounded window is enough to catch the
        // verbs and cannot run away.
        let window = &chunk[..chunk.len().min(400)];
        let mut verbs = BTreeSet::new();
        for verb in ["get", "post", "put", "patch", "delete"] {
            if window.contains(&format!("{verb}(")) {
                verbs.insert(verb.to_string());
            }
        }
        if !verbs.is_empty() {
            out.entry(path.to_string()).or_default().extend(verbs);
        }
    }
    assert!(
        out.len() > 30,
        "the router parse matched only {} paths — the `.route(...)` shape in \
         src/control/api.rs has changed and this test is now blind",
        out.len()
    );
    out
}

/// Every data-plane endpoint, from the constant that decides them.
fn proxied_suffixes() -> BTreeSet<String> {
    let src = source("src/proxy.rs");
    let start = src
        .find("const PROXIED_SUFFIXES")
        .expect("PROXIED_SUFFIXES in src/proxy.rs");
    let end = src[start..].find("];").expect("end of PROXIED_SUFFIXES") + start;
    let block = &src[start..end];
    let mut out = BTreeSet::new();
    for line in block.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('"') {
            if let Some(name) = rest.split('"').next() {
                out.insert(name.to_string());
            }
        }
    }
    assert!(!out.is_empty(), "no proxied suffixes parsed");
    out
}

/// `(method, subpath)` from `proxy::MCP_ROUTES`, parsed from the source the
/// dispatch actually uses.
fn mcp_routes() -> Vec<(String, String)> {
    let src = source("src/proxy.rs");
    let start = src
        .find("pub const MCP_ROUTES")
        .expect("MCP_ROUTES in src/proxy.rs");
    let end = src[start..].find("];").expect("end of MCP_ROUTES") + start;
    let mut out = Vec::new();
    for line in src[start..end].lines() {
        let line = line.trim();
        if !line.starts_with('(') {
            continue;
        }
        let parts: Vec<&str> = line.split('"').collect();
        if parts.len() >= 4 {
            out.push((parts[1].to_string(), parts[3].to_string()));
        }
    }
    assert!(!out.is_empty(), "no MCP routes parsed");
    out
}

/// The agent routes the spec describes must be dispatched by `proxy.rs`.
///
/// Weaker than the `MCP_ROUTES` check because the paths are templated, but it
/// still fails if `agent_route` stops handling one of the two shapes.
#[test]
fn the_agent_routes_the_spec_describes_are_dispatched() {
    let src = source("src/proxy.rs");
    assert!(
        src.contains("fn agent_route("),
        "the spec describes /v1/agents/{{name}} but proxy.rs has no agent_route"
    );
    for needle in [".well-known/agent-card.json", ".well-known/agent.json"] {
        assert!(
            src.contains(needle),
            "the spec describes the agent card but proxy.rs never matches {needle}"
        );
    }
}

/// Every MCP route in that list must also be in the spec, in the direction the
/// admin check already covers for `/admin/*`.
#[test]
fn every_mcp_route_is_in_the_spec() {
    let spec = spec_paths();
    let mut missing = Vec::new();
    for (method, subpath) in mcp_routes() {
        let path = format!("/v1{subpath}");
        let verb = method.to_lowercase();
        if !spec.get(&path).is_some_and(|v| v.contains(&verb)) {
            missing.push(format!("{method} {path}"));
        }
    }
    assert!(
        missing.is_empty(),
        "MCP routes absent from openapi.json: {missing:?}"
    );
}

fn spec_paths() -> BTreeMap<String, BTreeSet<String>> {
    let raw = source("openapi.json");
    let v: serde_json::Value = serde_json::from_str(&raw).expect("openapi.json is valid JSON");
    v["paths"]
        .as_object()
        .expect("paths object")
        .iter()
        .map(|(p, ops)| {
            (
                p.clone(),
                ops.as_object()
                    .expect("operations object")
                    .keys()
                    .cloned()
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn every_admin_route_is_in_the_spec() {
    let spec = spec_paths();
    let mut missing = Vec::new();
    for (path, verbs) in router_routes() {
        for verb in verbs {
            let listed = spec.get(&path).is_some_and(|v| v.contains(&verb));
            if !listed {
                missing.push(format!("{} {}", verb.to_uppercase(), path));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these routes exist but are not in openapi.json:\n  {}\n\
         Regenerate the spec, or the published API description is a lie about \
         what this server serves.",
        missing.join("\n  ")
    );
}

#[test]
fn every_data_plane_endpoint_is_in_the_spec() {
    let spec = spec_paths();
    let mut missing = Vec::new();
    for suffix in proxied_suffixes() {
        let path = format!("/v1{suffix}");
        if !spec.get(&path).is_some_and(|v| v.contains("post")) {
            missing.push(path);
        }
    }
    assert!(
        missing.is_empty(),
        "proxied endpoints absent from openapi.json: {missing:?}"
    );
}

/// The direction people forget: a spec entry outliving its route.
#[test]
fn the_spec_describes_no_route_that_does_not_exist() {
    let router = router_routes();
    let proxied: BTreeSet<String> = proxied_suffixes()
        .iter()
        .map(|s| format!("/v1{s}"))
        .collect();
    // Data-plane routes live in `proxy.rs`'s dispatch rather than an axum
    // router, so they are checked against their own sources. The MCP routes
    // are read from `MCP_ROUTES` rather than repeated here: a second
    // hand-maintained list is the drift this whole file exists to catch.
    let mut data_plane: BTreeSet<String> = ["/v1/models", "/health", "/metrics"]
        .into_iter()
        .map(str::to_string)
        .collect();
    data_plane.extend(mcp_routes().into_iter().map(|(_, p)| format!("/v1{p}")));
    // Templated by agent name, so there is no literal path to enumerate from
    // a const the way `MCP_ROUTES` is. Listed here and asserted below to be
    // dispatched, which is the closest thing to deriving them.
    data_plane.extend(
        [
            "/v1/agents/{name}",
            "/v1/agents/{name}/.well-known/agent-card.json",
        ]
        .into_iter()
        .map(str::to_string),
    );

    let mut stale = Vec::new();
    for (path, verbs) in spec_paths() {
        if proxied.contains(&path) || data_plane.contains(&path) {
            continue;
        }
        for verb in verbs {
            if !router.get(&path).is_some_and(|v| v.contains(&verb)) {
                stale.push(format!("{} {}", verb.to_uppercase(), path));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "openapi.json describes routes the server does not serve:\n  {}",
        stale.join("\n  ")
    );
}

/// Anything the binary embeds from outside `src/` must be copied into the
/// Docker build context, or the image build fails while every local build
/// succeeds.
///
/// That asymmetry is the whole point of this test. `cargo build` sees the
/// whole checkout, so a missing `COPY` is invisible until a release build —
/// which is exactly when it was found: `include_str!("../../openapi.json")`
/// compiled here and failed in the container, after CI had already spent
/// twenty minutes getting to it.
#[test]
fn everything_embedded_from_outside_src_is_copied_into_the_image() {
    let dockerfile = source("Dockerfile");
    let mut missing = Vec::new();

    // One file embeds from outside `src/` today. Scanning that file rather
    // than the whole tree keeps this cheap and, more importantly, honest: a
    // new `include_str!` elsewhere is not covered, and pretending otherwise
    // would make this test read as a guarantee it does not give.
    let src = source("src/control/api.rs");
    for m in src.match_indices("include_str!(\"../../") {
        let rest = &src[m.0 + "include_str!(\"../../".len()..];
        let Some(end) = rest.find('"') else { continue };
        let file = &rest[..end];
        if !dockerfile.contains(file) {
            missing.push(file.to_string());
        }
    }

    assert!(
        missing.is_empty(),
        "these files are embedded with include_str! but never COPYied in the \
         Dockerfile, so `cargo build` succeeds and the image build fails: {missing:?}"
    );
}
