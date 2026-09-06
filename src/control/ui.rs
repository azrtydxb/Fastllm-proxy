//! Embedded management UI (P4).
//!
//! `web/dist/` is the built SPA (see `web/`'s own README and the
//! `Dockerfile`'s `node` stage): `rust-embed` walks that directory at
//! **compile time** and bakes every file it finds into the binary, so the
//! deployed artefact is still one container image with no second thing to
//! build or ship alongside it.
//!
//! Two build-time properties this module exists to guarantee, both required
//! by `TODO.md`'s original design note:
//!
//! - **`cargo build`/`cargo test` never require Node.** Nothing here shells
//!   out to `npm`; `rust-embed` only ever reads whatever is already on disk
//!   under `web/dist/` when `cargo build` runs. The directory itself must
//!   exist (an empty directory, `web/dist/.gitkeep`, is committed for
//!   exactly this reason — `rust-embed`'s derive macro walks the path at
//!   compile time and a missing directory is a build error, an empty one is
//!   not), but its *contents* are never required.
//! - **A missing/empty `web/dist/` degrades to "UI not available", not a
//!   build failure or a panic.** `serve_asset` below checks for `index.html`
//!   specifically and returns a plain 503 with an explanatory body if it is
//!   absent — the state of every `cargo build` that has not first run `npm
//!   run build` in `web/`, and every `--no-default-features` proxy-only
//!   build, which links none of this at all (the whole module is behind the
//!   `control` feature, same as the admin API it is mounted alongside).
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

/// A conservative, self-contained-SPA CSP: everything the page needs
/// (scripts, styles, the `fetch` calls to `/admin/*`) is same-origin, so
/// `'self'` covers all of it with no `'unsafe-inline'`/`'unsafe-eval'`
/// carve-out for scripts. `img-src` allows `data:` for any inlined icon/
/// avatar the UI embeds as a data URI. This is defence in depth for the
/// admin UI specifically — the one surface in this codebase that renders
/// values (principal names, model names, ...) an operator typed into a
/// browser DOM — not a substitute for the UI code itself escaping what it
/// renders.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; \
     style-src 'self'; img-src 'self' data:; connect-src 'self'; \
     base-uri 'none'; frame-ancestors 'none'; object-src 'none'";

/// The `Router::fallback` for `control::api::serve`'s admin app: anything
/// not matched by an `/admin/*`, `/login`, `/snapshot`, `/usage` or
/// `/limits/reconcile` route falls through here. Serves the requested path
/// verbatim if `web/dist/` has it (`/assets/index-abc123.js` and friends),
/// and falls back to `index.html` for everything else — the standard
/// single-page-app rule, since the SPA's own client-side router resolves
/// `/models`, `/keys`, etc. from `index.html`, not from a file that exists
/// per route on disk.
///
/// A path under `/admin/*` is the one exception to that fallback rule: the
/// SPA's own client-side routes are never prefixed with `/admin` (see the
/// example above — `/models`, `/keys`), so a request that reaches this
/// function *and* starts with `/admin/` is unambiguously a caller
/// addressing the API, not the UI — a typo'd id, a deleted resource, a
/// route that was renamed or never existed. Falling back to `index.html`
/// for that would silently answer 200 with an HTML body where the caller
/// expected JSON (or a 404), swallowing the mistake instead of reporting
/// it. `admin_routes` (`control::api::serve`) is mounted ahead of this
/// fallback, so any real `/admin/*` route already matched before axum ever
/// reaches here — everything that does reach here under that prefix is, by
/// construction, not a real route.
pub async fn serve_asset(uri: Uri, headers: header::HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(file) = Assets::get(path) {
        return with_security_headers(served(file, true, if_none_match.as_deref()));
    }
    if path == "admin" || path.starts_with("admin/") {
        return with_security_headers(
            (
                StatusCode::NOT_FOUND,
                "no such /admin/* route; this path matched neither the admin API nor a static UI asset",
            )
                .into_response(),
        );
    }
    // Only the root gets the SPA. Everything else that reached here is a path
    // this server does not serve, and should say so.
    //
    // The UI routes on the *hash* (`/#/models`), so its deep links all have
    // `/` as their path — it has never needed a catch-all, and having one
    // meant every unknown path answered 200 with HTML. That is the same
    // defect as an error message asserting a cause it has not checked: a
    // client probing `/openapi.json` against a build too old to have it got a
    // success and a page of markup, which is a worse answer than a 404
    // because it looks like it worked.
    if !(path.is_empty() || path == "index.html") {
        return with_security_headers(
            (
                StatusCode::NOT_FOUND,
                format!(
                    "no route for /{path} on the admin port. The management UI is at /, the \
                     admin API under /admin/*, the OpenAPI description at /openapi.json, and \
                     the control-plane protocol at /snapshot and /usage."
                ),
            )
                .into_response(),
        );
    }
    match Assets::get("index.html") {
        Some(file) => with_security_headers(served(file, false, if_none_match.as_deref())),
        None => with_security_headers(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "management UI not available: web/dist/ was empty when this binary was built \
                 (run `npm run build` in web/, or use the image built from the project Dockerfile, \
                 which does this for you)",
            )
                .into_response(),
        ),
    }
}

/// `is_hashed_asset`: Vite (`web/vite.config.js`) fingerprints every built
/// asset's filename with a content hash (`index-abc123.js`), so a given URL
/// only ever serves one immutable body for the life of that hash — safe to
/// cache for a year and never revalidate. `index.html` (and the
/// SPA-fallback copy of it) is the opposite: it is what *names* the current
/// hashed assets, so caching it at all would pin a browser to a stale asset
/// manifest after the next deploy. `no-cache` (revalidate every time, not
/// `no-store`) lets a conditional request save the transfer when nothing has
/// changed — but only because of the `ETag` below.
///
/// Without a validator, `no-cache` promises a revalidation the browser has no
/// way to perform, and browsers resolve that by serving the cached copy. The
/// symptom is the worst kind: the UI deploys, the server serves a new
/// `index.html` naming new hashed assets, and anyone holding the old shell
/// keeps loading the old bundle — which *is* still there, because hashed
/// assets are immutable and cached for a year. Every UI change is invisible to
/// exactly the people already using it, and a hard reload is the only cure. It
/// took a deploy that "did nothing" to notice.
///
/// The tag is the content hash `rust_embed` already computed at build time, so
/// it costs nothing and cannot drift from the bytes it names.
fn served(
    file: rust_embed::EmbeddedFile,
    is_hashed_asset: bool,
    if_none_match: Option<&str>,
) -> Response {
    let mime = file.metadata.mimetype();
    let cache_control = if is_hashed_asset {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let etag = format!("\"{}\"", hex(file.metadata.sha256_hash()));
    if if_none_match.is_some_and(|v| v.split(',').any(|t| t.trim() == etag)) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, cache_control.to_string()),
                (header::ETAG, etag),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CACHE_CONTROL, cache_control.to_string()),
            (header::ETAG, etag),
        ],
        file.data,
    )
        .into_response()
}

fn hex(bytes: [u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `X-Content-Type-Options: nosniff` and the CSP above, applied uniformly
/// to every response this module returns — including the 404/503 error
/// bodies, which are plain text but cost nothing to cover with the same
/// headers rather than carving out an exception.
fn with_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property that matters most about this module: it must never
    /// panic or fail to compile just because a plain `cargo build` (no
    /// prior `npm run build`) leaves `web/dist/` empty. This test runs in
    /// that exact state in CI (nothing here shells out to node), so seeing
    /// it pass is direct evidence a bare `web/dist/.gitkeep` degrades
    /// gracefully rather than merely being argued to.
    #[tokio::test]
    async fn the_root_serves_the_ui_or_says_why_it_cannot() {
        let uri: Uri = "/".parse().unwrap();
        let resp = serve_asset(uri, header::HeaderMap::new()).await;
        // Either this repo's checkout has a built `web/dist/index.html`
        // (200, whatever the SPA's shell is) or it does not (503, the
        // explanatory body above) — both are "did not panic and said
        // something legible", which is the whole contract.
        assert!(
            resp.status() == StatusCode::OK || resp.status() == StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// An unknown path must 404 rather than answering 200 with the UI shell.
    ///
    /// This used to be the opposite, and the consequence was worse than
    /// untidy: a client probing `/openapi.json` against a build too old to
    /// serve it got a 200 and a page of HTML. A success that means "not
    /// found" is harder to diagnose than a 404, because the caller has no
    /// reason to look further.
    ///
    /// Safe because the UI routes on the *hash* — `/#/models` has `/` as its
    /// path — so no client-side route ever arrives here as a path to be
    /// resolved.
    #[tokio::test]
    async fn an_unknown_path_404s_rather_than_serving_the_ui_shell() {
        for path in [
            "/openapi.json",
            "/some/spa/route",
            "/snapshot-typo",
            "/v1/models",
        ] {
            let uri: Uri = path.parse().unwrap();
            let resp = serve_asset(uri, header::HeaderMap::new()).await;
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} reached the asset fallback and must 404, not answer 200 with HTML"
            );
        }
    }

    /// The fix for the SPA fallback swallowing an unmatched `/admin/*`
    /// path: a request under that prefix that reaches this fallback (i.e.
    /// did not match any real route in `admin_routes`) must 404, not answer
    /// 200 with the UI shell — the SPA's own client routes are never
    /// prefixed with `/admin` (see `serve_asset`'s doc comment), so
    /// anything under it here is unambiguously a bad API call, not a
    /// client-side route the SPA is about to resolve.
    #[tokio::test]
    async fn an_unmatched_admin_path_404s_instead_of_falling_back_to_the_ui() {
        for path in ["/admin", "/admin/", "/admin/no-such-route"] {
            let uri: Uri = path.parse().unwrap();
            let resp = serve_asset(uri, header::HeaderMap::new()).await;
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{path} must 404, not fall back to index.html"
            );
        }
    }

    /// The premise this replaces was wrong, which is why it is written out
    /// rather than deleted: it asserted that `/models` was "a genuine SPA
    /// client route" that must not 404, and therefore justified a catch-all
    /// answering 200 for every unknown path.
    ///
    /// The UI has never routed on the path. `web/src/App.jsx` reads
    /// `window.location.hash` and writes `#/models`, so every one of its
    /// routes arrives here as `/` and `/models` is not a UI route at all —
    /// it is a request for something this server does not have.
    ///
    /// So the fallback needs exactly one path, and this pins that: a deep
    /// link into the UI is served, because its path is the root.
    #[tokio::test]
    async fn a_hash_route_is_served_because_its_path_is_the_root() {
        // What the browser actually requests for `https://host/#/models`.
        let uri: Uri = "/".parse().unwrap();
        let resp = serve_asset(uri, header::HeaderMap::new()).await;
        assert_ne!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "the root must serve the UI (200) or the not-built placeholder (503), never 404"
        );
    }

    /// Every response `serve_asset` returns — success, 404 or 503 — carries
    /// the same two security headers, per this module's doc comment on
    /// `with_security_headers`.
    #[tokio::test]
    async fn every_response_carries_nosniff_and_a_csp() {
        for path in ["/", "/admin/no-such-route", "/some/spa/route"] {
            let uri: Uri = path.parse().unwrap();
            let resp = serve_asset(uri, header::HeaderMap::new()).await;
            assert_eq!(
                resp.headers().get(header::X_CONTENT_TYPE_OPTIONS),
                Some(&HeaderValue::from_static("nosniff")),
                "{path} is missing X-Content-Type-Options: nosniff"
            );
            assert!(
                resp.headers().contains_key(header::CONTENT_SECURITY_POLICY),
                "{path} is missing a Content-Security-Policy"
            );
        }
    }

    /// `index.html` (and its SPA-fallback stand-in) must never be cached —
    /// it is what names the currently-deployed hashed asset filenames, so a
    /// browser holding a stale copy after a deploy would go on requesting
    /// asset hashes that no longer exist. A concrete, in-memory
    /// `EmbeddedFile` (rather than depending on `web/dist/` being built) is
    /// what makes this test meaningful regardless of whether this checkout
    /// has run `npm run build`.
    /// `no-cache` without a validator is a promise the browser cannot keep.
    ///
    /// This is the bug that made a UI deploy invisible: the shell said
    /// "revalidate before using me" and gave the browser nothing to revalidate
    /// with, so browsers served the cached copy — which named hashed assets
    /// that are, correctly, cached for a year. The new bundle was on the
    /// server and nobody already using the UI could reach it.
    #[test]
    fn the_shell_carries_a_validator_and_answers_a_conditional_request() {
        let file = rust_embed::EmbeddedFile {
            data: std::borrow::Cow::Borrowed(b"<html></html>"),
            metadata: rust_embed::Metadata::__rust_embed_new([7; 32], None, None, "text/html"),
        };
        let fresh = served(file.clone(), false, None);
        let etag = fresh
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("index.html must carry an ETag, or `no-cache` cannot revalidate")
            .to_string();
        assert_eq!(fresh.status(), StatusCode::OK);

        // The same tag back means the browser already has these bytes.
        let repeat = served(file.clone(), false, Some(&etag));
        assert_eq!(repeat.status(), StatusCode::NOT_MODIFIED);

        // A tag from an older build does not, which is the case that matters:
        // it is how a stale shell gets replaced.
        let stale = served(file, false, Some("\"0000\""));
        assert_eq!(stale.status(), StatusCode::OK);
    }

    #[test]
    fn index_html_is_never_cached_but_a_hashed_asset_is_cached_immutably() {
        let file = rust_embed::EmbeddedFile {
            data: std::borrow::Cow::Borrowed(b"<html></html>"),
            metadata: rust_embed::Metadata::__rust_embed_new([0; 32], None, None, "text/html"),
        };
        let index_resp = served(file.clone(), false, None);
        assert_eq!(
            index_resp
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache"),
            "index.html must revalidate every time, never be cached blindly"
        );

        let asset_resp = served(file, true, None);
        let cache_control = asset_resp
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            cache_control.contains("immutable") && cache_control.contains("max-age"),
            "a content-hashed asset should be cached long-term and immutably, got {cache_control:?}"
        );
    }
}
