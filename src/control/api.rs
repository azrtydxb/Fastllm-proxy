//! Admin API and the snapshot endpoint.
//!
//! `/snapshot` is read-only and authenticated with the proxy's own token,
//! which is distinct from any user key: a stolen proxy token discloses policy
//! — key hashes, never plaintext — and grants nothing else.

use crate::control::build::build_snapshot;
use crate::snapshot::{constant_time_eq, hash_key, Snapshot};
use arc_swap::ArcSwap;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;

/// 32 bytes of OS randomness. High entropy is what makes SHA-256 the right
/// hash for these; do not shorten it.
pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("sk-{}", hex::encode(bytes))
}

/// Where a freshly built snapshot lands when the admin API writes one.
///
/// One trait, one implementor per role, so it is impossible to update the
/// snapshot without also updating whatever else must never be allowed to
/// drift from it. `--role control` has nothing else to keep in sync, so
/// `ArcSwap<Snapshot>` alone is a sink. `--role all` implements this on
/// `AppState`, where storing a snapshot always also rebuilds the routing
/// `Registry` from it (see `AppState::apply_snapshot`). There is
/// deliberately no second call site here that also has to remember to
/// rebuild the registry — `refresh` below only ever calls this one method.
pub trait SnapshotSink: Send + Sync {
    fn store_snapshot(&self, snap: Snapshot) -> Arc<Snapshot>;
    fn current_snapshot(&self) -> Arc<Snapshot>;
}

impl SnapshotSink for ArcSwap<Snapshot> {
    fn store_snapshot(&self, snap: Snapshot) -> Arc<Snapshot> {
        let arc = Arc::new(snap);
        self.store(Arc::clone(&arc));
        arc
    }

    fn current_snapshot(&self) -> Arc<Snapshot> {
        self.load_full()
    }
}

/// Enough to identify a key in a list, far too little to guess it.
pub fn display_prefix(key: &str) -> String {
    key.chars().take(11).collect()
}

pub async fn create_key(
    pool: &PgPool,
    name: &str,
    principal_id: i64,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> anyhow::Result<(String, i64)> {
    let plaintext = generate_key();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO api_keys (hash, prefix, name, principal_id, expires_at)
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(hash_key(&plaintext).to_vec())
    .bind(display_prefix(&plaintext))
    .bind(name)
    .bind(principal_id)
    .bind(expires_at)
    .fetch_one(pool)
    .await?;
    Ok((plaintext, id))
}

#[derive(Clone)]
struct Ctx {
    pool: PgPool,
    proxy_token: String,
    cache: Arc<dyn SnapshotSink>,
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
    principal_id: i64,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Serialize)]
struct NewKeyResponse {
    id: i64,
    /// Returned exactly once. There is no way to retrieve it again.
    key: String,
}

async fn post_key(
    State(ctx): State<Ctx>,
    Json(body): Json<NewKey>,
) -> Result<Json<NewKeyResponse>, StatusCode> {
    let (key, id) = create_key(&ctx.pool, &body.name, body.principal_id, body.expires_at)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    refresh(&ctx).await;
    Ok(Json(NewKeyResponse { id, key }))
}

async fn revoke_key(State(ctx): State<Ctx>, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    sqlx::query("UPDATE api_keys SET disabled = TRUE WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Rebuild immediately after a write so revocation is bounded by the proxy's
/// poll interval alone, not by poll interval plus rebuild interval.
async fn refresh(ctx: &Ctx) {
    if let Ok(snap) = build_snapshot(&ctx.pool).await {
        ctx.cache.store_snapshot(snap);
    }
}

async fn get_snapshot(State(ctx): State<Ctx>, headers: HeaderMap) -> impl IntoResponse {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    // `/snapshot` is gated on this comparison alone, and what it discloses
    // (every key hash, plus usable upstream backend credentials per the
    // schema comment on `model_backends.upstream_api_key`) makes it worth
    // paying for a non-short-circuiting compare rather than plain `==`.
    let authorised = match presented {
        Some(token) => constant_time_eq(token.as_bytes(), ctx.proxy_token.as_bytes()),
        None => false,
    };
    if !authorised {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let snap = ctx.cache.current_snapshot();
    let etag = format!("\"{}\"", snap.version);
    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    ([("etag", etag)], Json(snap.as_ref().to_wire())).into_response()
}

pub async fn serve(
    pool: PgPool,
    addr: SocketAddr,
    proxy_token: String,
    cache: Arc<dyn SnapshotSink>,
) -> anyhow::Result<()> {
    let ctx = Ctx {
        pool,
        proxy_token,
        cache,
    };
    let app = Router::new()
        .route("/admin/keys", post(post_key))
        .route("/admin/keys/{id}", delete(revoke_key))
        .route("/snapshot", get(get_snapshot))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_is_random_prefixed_and_long_enough() {
        let a = generate_key();
        let b = generate_key();
        assert_ne!(a, b);
        assert!(a.starts_with("sk-"));
        // 32 random bytes hex-encoded, plus the prefix.
        assert_eq!(a.len(), 3 + 64);
    }

    #[test]
    fn the_display_prefix_never_reveals_enough_to_guess() {
        let key = generate_key();
        let prefix = display_prefix(&key);
        assert_eq!(prefix.len(), 11); // "sk-" + 8
        assert!(key.starts_with(&prefix));
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn creating_a_key_returns_plaintext_once_and_stores_only_the_hash() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let principal_id: i64 = sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(format!(
            "task6-test-principal-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        let (plaintext, id) = create_key(&pool, "test-key", principal_id, None)
            .await
            .unwrap();
        let stored: Vec<u8> = sqlx::query_scalar("SELECT hash FROM api_keys WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored, crate::snapshot::hash_key(&plaintext).to_vec());
        let any_plaintext: Option<String> =
            sqlx::query_scalar("SELECT prefix FROM api_keys WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(
            any_plaintext.unwrap(),
            plaintext,
            "plaintext must not be stored"
        );
    }
}
