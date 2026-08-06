//! Admin API and the snapshot endpoint.
//!
//! `/snapshot` is read-only and authenticated with the proxy's own token,
//! which is distinct from any user key: a stolen proxy token discloses policy
//! — key hashes, never plaintext — and grants nothing else.

use crate::control::build::build_snapshot;
use crate::control::secrets::EncryptionKey;
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

/// `sqlx::Error`, not `anyhow::Result`, specifically so `post_key` can tell a
/// bad `principal_id` (a foreign key violation, and the caller's mistake)
/// apart from every other failure mode without losing the structured
/// database error `anyhow` would have erased.
pub async fn create_key(
    pool: &PgPool,
    name: &str,
    principal_id: i64,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(String, i64), sqlx::Error> {
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
    key: Arc<EncryptionKey>,
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
) -> Result<Json<NewKeyResponse>, (StatusCode, Json<serde_json::Value>)> {
    let (key, id) = create_key(&ctx.pool, &body.name, body.principal_id, body.expires_at)
        .await
        .map_err(|e| key_creation_error(&e, body.principal_id))?;
    refresh(&ctx).await;
    Ok(Json(NewKeyResponse { id, key }))
}

/// Turn a failed key insert into a message that says what was wrong, instead
/// of the bare 400 this used to be (`map_err(|_| StatusCode::BAD_REQUEST)`).
/// An operator hitting a foreign key violation because `principal_id` does
/// not exist should not have to go read `create_key`'s SQL to find that out —
/// especially now that a fresh install ships exactly one principal (see
/// `migrations/0003_bootstrap_principal_and_operator_grants.sql`) and every
/// other id really is a typo.
fn key_creation_error(e: &sqlx::Error, principal_id: i64) -> (StatusCode, Json<serde_json::Value>) {
    let is_missing_principal = matches!(
        e,
        sqlx::Error::Database(db) if db.is_foreign_key_violation()
    );
    if is_missing_principal {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "no principal with id {principal_id}; the bootstrap principal seeded by \
                     migrations is id 1 — create another with INSERT INTO principals before \
                     minting a key against it"
                )
            })),
        )
    } else {
        tracing::error!(error = %e, principal_id, "key creation failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "key creation failed; see server logs" })),
        )
    }
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
    if let Ok(snap) = build_snapshot(&ctx.pool, &ctx.key).await {
        ctx.cache.store_snapshot(snap);
    }
}

/// One rebuild-and-maybe-publish cycle: build the current database state and
/// compare it against what is already published, publishing only if the
/// content actually differs.
///
/// Factored out of [`spawn_snapshot_rebuilder`] so a test can drive one cycle
/// deterministically instead of racing a `tokio::time::interval`. Goes through
/// the exact same [`SnapshotSink::store_snapshot`] the admin API's `refresh`
/// uses, so whatever invariant a role's `SnapshotSink` implementation
/// maintains (in `--role all`, "storing a snapshot always rebuilds the
/// routing `Registry` from it") holds here too — there is no second,
/// almost-identical write path for this to drift from.
async fn rebuild_once(
    pool: &PgPool,
    cache: &dyn SnapshotSink,
    key: &EncryptionKey,
) -> anyhow::Result<()> {
    let next = build_snapshot(pool, key).await?;
    let current = cache.current_snapshot();
    if current.content_eq(&next) {
        tracing::debug!(version = current.version, "snapshot rebuild: no changes");
        return Ok(());
    }
    let from = current.version;
    let stored = cache.store_snapshot(next);
    tracing::info!(from, to = stored.version, "snapshot rebuilt from database");
    Ok(())
}

/// Keep a running control plane's snapshot current with rows changed by
/// anything *other* than its own admin API — `fastllm-proxy import` (a
/// separate process) or a hand-written `UPDATE`/`INSERT` against Postgres,
/// both of which the outage-recovery procedure in `deploy/README.md`
/// documents as valid ways to fix a moved backend. Before this existed,
/// `build_snapshot` only ran at process startup and after the admin API's own
/// writes (`refresh`, above), so neither of those reached a running control
/// plane at all: the documented procedure silently did nothing until the pod
/// was restarted.
///
/// `interval` follows the same "0 disables" convention as `--config-poll`.
pub fn spawn_snapshot_rebuilder(
    pool: PgPool,
    cache: Arc<dyn SnapshotSink>,
    interval: std::time::Duration,
    key: Arc<EncryptionKey>,
) {
    if interval.is_zero() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = rebuild_once(&pool, cache.as_ref(), &key).await {
                tracing::warn!(error = %e, "periodic snapshot rebuild failed; keeping previous snapshot");
            }
        }
    });
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
    key: Arc<EncryptionKey>,
) -> anyhow::Result<()> {
    let ctx = Ctx {
        pool,
        proxy_token,
        cache,
        key,
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

    /// Shared with `control::import::tests` — see `secrets::test_key` for
    /// why every DB-backed test in `control::*` must use the same key
    /// rather than each picking its own.
    use crate::control::secrets::test_key;

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

    /// Regression for the review finding: a bad `principal_id` used to
    /// surface as an empty 400 (`map_err(|_| StatusCode::BAD_REQUEST)`).
    /// `key_creation_error` must recognise the foreign key violation and say
    /// so, not fall through to the generic 500 branch.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_nonexistent_principal_id_reports_what_was_wrong() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();

        let bogus_principal_id = -1;
        let err = create_key(&pool, "test-key", bogus_principal_id, None)
            .await
            .expect_err("a principal_id that does not exist must fail");

        let (status, body) = key_creation_error(&err, bogus_principal_id);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body.0["error"].as_str().unwrap();
        assert!(
            message.contains("principal"),
            "the message must say what was wrong, not just fail silently: {message}"
        );
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

    /// The regression this task's Critical review finding described:
    /// `import` runs in a separate process, and the documented outage-fix is
    /// a hand-written `UPDATE` against Postgres — neither goes through the
    /// admin API's `refresh()`, so before `rebuild_once`/
    /// `spawn_snapshot_rebuilder` existed, a row changed either way never
    /// reached a *running* control plane's published snapshot at all.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_snapshot_rebuilt_from_changed_database_rows_is_published() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let cache: Arc<dyn SnapshotSink> = Arc::new(ArcSwap::from_pointee(Snapshot::default()));

        // Establish a baseline as of right now.
        rebuild_once(&pool, cache.as_ref(), &test_key())
            .await
            .unwrap();
        let before = cache.current_snapshot();

        // Simulate `import` (a separate process) or the manual `UPDATE`
        // `deploy/README.md` documents for a moved backend — a write that
        // never touches the admin API's own `refresh()`.
        let name = format!(
            "rebuild-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        sqlx::query("INSERT INTO models (name) VALUES ($1)")
            .bind(&name)
            .execute(&pool)
            .await
            .unwrap();

        rebuild_once(&pool, cache.as_ref(), &test_key())
            .await
            .unwrap();
        let after = cache.current_snapshot();

        // Not `assert_ne!(before.version, after.version)`: `version` is
        // `EXTRACT(EPOCH FROM now())::BIGINT`, one-second resolution, and
        // this whole test runs well under a second — two real, distinct
        // snapshots can legitimately carry the same timestamp. A new `Arc`
        // was published (rather than `rebuild_once` deciding nothing
        // changed) is the property that actually matters here.
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a real content change must publish a new snapshot"
        );
        assert!(
            after.models.iter().any(|m| m.name == name),
            "a model inserted outside the admin API must reach a running control plane's snapshot"
        );
    }
}
