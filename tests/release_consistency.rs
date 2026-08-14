//! Every place the released version is written, against `Cargo.toml`.
//!
//! # Why this is a test and not a checklist
//!
//! Cutting a release touches fifteen files: the two crates, the Helm chart's
//! two version fields, the kustomize base, the operator's CRD default and its
//! generated manifest, four compose and manifest images, and the docs that
//! show a `docker run`. Missing one does not fail anything — it produces an
//! install path that quietly deploys the *previous* release, which is the
//! failure that shipped `v0.1.0` manifests describing MCP routes `v0.1.0` did
//! not serve.
//!
//! This does not check the git tag. The tag is created last and this has to
//! pass before it exists; `Cargo.toml` is the source of truth and the tag is
//! made from a commit that already agrees with it.

use std::fs;
use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// `version = "x.y.z"` from the workspace root's `[package]`.
fn crate_version() -> String {
    read("Cargo.toml")
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|v| v.split('"').next())
        .expect("version in Cargo.toml")
        .to_string()
}

/// Files that name the released image or chart, and must all agree.
const VERSIONED: &[&str] = &[
    "charts/fastllm-proxy/Chart.yaml",
    "charts/fastllm-proxy/README.md",
    "deploy/docker-compose.split.yml",
    "deploy/kubernetes/README.md",
    "deploy/kubernetes/base/control.yaml",
    "deploy/kubernetes/base/proxy.yaml",
    "deploy/kubernetes/base/kustomization.yaml",
    "docs/README-book.md",
    "docs/operations/shapes.md",
    "operator/Cargo.toml",
    "operator/Dockerfile",
    "operator/README.md",
    "operator/deploy/crd.yaml",
    "operator/deploy/example.yaml",
    "operator/deploy/operator.yaml",
];

/// Any `vN.N.N` that is not the current version, in a file that ships an
/// install path.
#[test]
fn every_install_path_points_at_the_current_release() {
    let want = crate_version();
    let mut stale = Vec::new();

    for file in VERSIONED {
        let body = read(file);
        for (n, line) in body.lines().enumerate() {
            // Only lines that actually name a release: a changelog-style
            // heading or a prose mention of an older version is legitimate
            // history, and this file does not police prose.
            let names_release = line.contains("ghcr.io/azrtydxb/fastllm-")
                || line.contains("newTag:")
                || line.contains("app.kubernetes.io/version:")
                || line.trim_start().starts_with("version:")
                || line.trim_start().starts_with("appVersion:")
                || line.trim_start().starts_with("version = \"")
                || line.contains("image.tag=");
            if !names_release {
                continue;
            }
            for found in versions_in(line) {
                if found != want {
                    stale.push(format!(
                        "{file}:{} names {found}, not {want}\n    {}",
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        stale.is_empty(),
        "these install paths point at a release that is not the current one:\n  {}\n\n\
         Cutting a release means bumping all of them together. An install path \
         left behind deploys the previous release and silently lacks whatever \
         this one added.",
        stale.join("\n  ")
    );
}

/// Bare `x.y.z` and `vx.y.z` occurrences in one line.
fn versions_in(line: &str) -> Vec<String> {
    let bytes: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut dots = 0;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
                if bytes[i] == '.' {
                    dots += 1;
                }
                i += 1;
            }
            let text: String = bytes[start..i].iter().collect();
            // `0.1.0` shaped, not a port, a byte count or a date.
            if dots == 2 && !text.ends_with('.') {
                out.push(text);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// The chart's own version and the app it deploys move together.
///
/// They are different numbers in principle — a chart can be revised without
/// the app changing — but this project releases them as one thing, and a
/// chart claiming to deploy an app version it does not is worse than the
/// coupling.
#[test]
fn the_chart_version_and_the_app_it_deploys_agree() {
    let chart = read("charts/fastllm-proxy/Chart.yaml");
    let field = |name: &str| {
        chart
            .lines()
            .find_map(|l| l.trim().strip_prefix(name))
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap_or_else(|| panic!("{name} in Chart.yaml"))
    };
    assert_eq!(
        field("version:"),
        field("appVersion:"),
        "Chart.yaml's version and appVersion disagree"
    );
    assert_eq!(field("appVersion:"), crate_version());
}

/// The operator ships as its own image on the same release.
#[test]
fn the_operator_crate_is_on_the_same_version() {
    let operator = read("operator/Cargo.toml");
    let version = operator
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|v| v.split('"').next())
        .expect("version in operator/Cargo.toml");
    assert_eq!(version, crate_version());
}
