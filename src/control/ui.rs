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
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist/"]
struct Assets;

/// The `Router::fallback` for `control::api::serve`'s admin app: anything
/// not matched by an `/admin/*`, `/login`, `/snapshot`, `/usage` or
/// `/limits/reconcile` route falls through here. Serves the requested path
/// verbatim if `web/dist/` has it (`/assets/index-abc123.js` and friends),
/// and falls back to `index.html` for everything else — the standard
/// single-page-app rule, since the SPA's own client-side router resolves
/// `/models`, `/keys`, etc. from `index.html`, not from a file that exists
/// per route on disk.
pub async fn serve_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if let Some(file) = Assets::get(path) {
        return served(file);
    }
    match Assets::get("index.html") {
        Some(file) => served(file),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "management UI not available: web/dist/ was empty when this binary was built \
             (run `npm run build` in web/, or use the image built from the project Dockerfile, \
             which does this for you)",
        )
            .into_response(),
    }
}

fn served(file: rust_embed::EmbeddedFile) -> Response {
    let mime = file.metadata.mimetype();
    ([(header::CONTENT_TYPE, mime)], file.data).into_response()
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
    async fn a_missing_asset_falls_back_to_index_html_or_a_clear_unavailable_response() {
        let uri: Uri = "/some/spa/route".parse().unwrap();
        let resp = serve_asset(uri).await;
        // Either this repo's checkout has a built `web/dist/index.html`
        // (200, whatever the SPA's shell is) or it does not (503, the
        // explanatory body above) — both are "did not panic and said
        // something legible", which is the whole contract.
        assert!(
            resp.status() == StatusCode::OK || resp.status() == StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
