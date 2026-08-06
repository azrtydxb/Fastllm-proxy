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

/// Shortest key for which taking [`display_prefix`]'s fixed 11 characters
/// still leaves enough unknown to be worth calling a prefix.
const MIN_PREFIXABLE_KEY_LEN: usize = 24;

/// [`display_prefix`] for keys this process did not mint.
///
/// `generate_key` produces 67 characters, so 11 of them is a prefix. A key
/// imported from a config file is whatever an operator wrote — `sk-eval` is
/// seven characters, and `display_prefix` would store the entire secret in a
/// column `GET /admin/keys` hands back. Below the threshold there is no
/// safe partial disclosure to make, so make none: the key's `name` is what
/// identifies it in a listing anyway.
pub fn safe_display_prefix(key: &str) -> String {
    if key.chars().count() >= MIN_PREFIXABLE_KEY_LEN {
        display_prefix(key)
    } else {
        "(hidden)".to_string()
    }
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

/// Every admin route fails the same way `post_key` does: a status plus a JSON
/// body that says what was wrong. `/admin/*` has no authentication and no UI
/// yet, so the error body is the entire diagnostic an operator gets — a bare
/// status code just means reading the SQL to find out which id was the typo.
type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// For failures the caller could not have caused and cannot act on. The
/// structured `sqlx::Error` goes to the log, not to the response: it can name
/// columns and constraints, and `/admin/*` is unauthenticated.
fn db_error(op: &str, e: &sqlx::Error) -> ApiError {
    tracing::error!(error = %e, op, "admin API database call failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("{op} failed; see server logs"),
    )
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_foreign_key_violation())
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
                     migrations is id 1 — GET /admin/principals lists the rest, and \
                     POST /admin/principals creates one"
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

async fn revoke_key(State(ctx): State<Ctx>, Path(id): Path<i64>) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("UPDATE api_keys SET disabled = TRUE WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("key revocation", &e))?;
    // An `UPDATE` that matched nothing is a successful statement, so the
    // previous `map_err(|_| NOT_FOUND)` reported 204 for a key id that does
    // not exist — the one case an operator most needs to hear about, since
    // "revoked" and "typo, still live" looked identical.
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no key with id {id}; GET /admin/keys lists the ids that exist"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Everything about a key except anything that could be used as one.
///
/// No `hash`: it is not a display value, it is the *verifier*. Anyone holding
/// it can neither derive the key (SHA-256) nor authenticate with it against
/// this proxy, but handing it out turns an offline dictionary attack against
/// a weak imported key into a thing that is possible at all, for no
/// operational benefit. `prefix` is the identifier a listing needs — see
/// `safe_display_prefix` for why that stays a prefix even for keys this
/// process did not mint.
#[derive(Serialize)]
struct KeyView {
    id: i64,
    prefix: String,
    name: String,
    principal_id: i64,
    principal: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    disabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

type KeyListRow = (
    i64,
    String,
    String,
    i64,
    String,
    Option<chrono::DateTime<chrono::Utc>>,
    bool,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

async fn list_keys(State(ctx): State<Ctx>) -> Result<Json<Vec<KeyView>>, ApiError> {
    let rows: Vec<KeyListRow> = sqlx::query_as(
        "SELECT k.id, k.prefix, k.name, k.principal_id, p.name, k.expires_at, k.disabled,
                k.created_at, k.last_used_at
         FROM api_keys k JOIN principals p ON p.id = k.principal_id
         ORDER BY k.id",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing keys", &e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(
                    id,
                    prefix,
                    name,
                    principal_id,
                    principal,
                    expires_at,
                    disabled,
                    created_at,
                    last_used_at,
                )| KeyView {
                    id,
                    prefix,
                    name,
                    principal_id,
                    principal,
                    expires_at,
                    disabled,
                    created_at,
                    last_used_at,
                },
            )
            .collect(),
    ))
}

#[derive(Serialize)]
struct PrincipalView {
    id: i64,
    kind: String,
    name: String,
    email: Option<String>,
    disabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    /// Roles, not flattened permissions: what a principal may *invoke* is
    /// answered by the snapshot (`build::flatten_grants`), and duplicating
    /// that resolution here would be a second implementation of the one
    /// question the whole design exists to answer in exactly one place.
    roles: Vec<String>,
}

type PrincipalRow = (
    i64,
    String,
    String,
    Option<String>,
    bool,
    chrono::DateTime<chrono::Utc>,
    Vec<String>,
);

async fn list_principals(State(ctx): State<Ctx>) -> Result<Json<Vec<PrincipalView>>, ApiError> {
    let rows: Vec<PrincipalRow> = sqlx::query_as(
        "SELECT p.id, p.kind, p.name, p.email, p.disabled, p.created_at,
                COALESCE(
                  ARRAY_AGG(r.name ORDER BY r.name) FILTER (WHERE r.name IS NOT NULL),
                  ARRAY[]::TEXT[]
                )
         FROM principals p
         LEFT JOIN principal_roles pr ON pr.principal_id = p.id
         LEFT JOIN roles r ON r.id = pr.role_id
         GROUP BY p.id
         ORDER BY p.id",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing principals", &e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, kind, name, email, disabled, created_at, roles)| PrincipalView {
                    id,
                    kind,
                    name,
                    email,
                    disabled,
                    created_at,
                    roles,
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewPrincipal {
    name: String,
    /// `service_account` by default: a `user` is only useful once it has a
    /// password, and password handling (Argon2id, sessions) lands with the
    /// management UI. Defaulting to the kind this route can actually
    /// finish creating beats accepting `user` silently and producing a
    /// principal nobody can sign in as.
    kind: Option<String>,
    email: Option<String>,
}

const PRINCIPAL_KINDS: [&str; 2] = ["user", "service_account"];

async fn post_principal(
    State(ctx): State<Ctx>,
    Json(body): Json<NewPrincipal>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let kind = body.kind.unwrap_or_else(|| "service_account".to_string());
    if !PRINCIPAL_KINDS.contains(&kind.as_str()) {
        // The CHECK constraint would catch this too, but as an opaque
        // database error rather than a sentence naming the two legal values.
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "kind {kind:?} is not a principal kind; use one of {}",
                PRINCIPAL_KINDS.join(", ")
            ),
        ));
    }
    if body.name.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must not be empty; it is how a principal is identified in listings and in \
             imported grants",
        ));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO principals (kind, name, email) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&kind)
    .bind(&body.name)
    .bind(&body.email)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "a principal with name {:?} (or that email) already exists; \
                     GET /admin/principals lists them",
                    body.name
                ),
            )
        } else {
            db_error("principal creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": body.name, "kind": kind })),
    ))
}

async fn delete_principal(
    State(ctx): State<Ctx>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // ON DELETE CASCADE takes this principal's keys and role grants with it
    // (migrations/0001), which is the point: a principal left behind with
    // live keys and no grants is a key that authenticates and authorises
    // nothing, which is harder to reason about than its absence.
    let done = sqlx::query("DELETE FROM principals WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("principal deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no principal with id {id}; GET /admin/principals lists the ids that exist"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RoleGrant {
    role: String,
}

async fn grant_role(
    State(ctx): State<Ctx>,
    Path(principal_id): Path<i64>,
    Json(body): Json<RoleGrant>,
) -> Result<StatusCode, ApiError> {
    // Resolved by name before the insert, rather than folded into one
    // `INSERT ... SELECT ... WHERE r.name = $2`, so "no such role", "no such
    // principal" and "already granted" stay three distinct answers instead
    // of collapsing into one zero-rows-affected.
    let role_id: Option<i64> = sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
        .bind(&body.role)
        .fetch_optional(&ctx.pool)
        .await
        .map_err(|e| db_error("role lookup", &e))?;
    let Some(role_id) = role_id else {
        let known: Vec<String> = sqlx::query_scalar("SELECT name FROM roles ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("role lookup", &e))?;
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "no role named {:?}; known roles are {}",
                body.role,
                known.join(", ")
            ),
        ));
    };
    sqlx::query(
        "INSERT INTO principal_roles (principal_id, role_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(principal_id)
    .bind(role_id)
    .execute(&ctx.pool)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "no principal with id {principal_id}; GET /admin/principals lists the ids \
                     that exist"
                ),
            )
        } else {
            db_error("role grant", &e)
        }
    })?;
    refresh(&ctx).await;
    // Deliberately 204 whether or not the row was new: granting a role that
    // is already granted has arrived at the requested state, and reporting a
    // conflict would make a retried request look like a failure.
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_role(
    State(ctx): State<Ctx>,
    Path((principal_id, role)): Path<(i64, String)>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query(
        "DELETE FROM principal_roles pr USING roles r
         WHERE pr.role_id = r.id AND pr.principal_id = $1 AND r.name = $2",
    )
    .bind(principal_id)
    .bind(&role)
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("role revocation", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!(
                "principal {principal_id} does not hold a role named {role:?}; \
                 GET /admin/principals lists each principal's roles"
            ),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct BackendView {
    id: i64,
    api_base: String,
    upstream_model: String,
    /// Whether a credential is configured — never the credential itself, in
    /// either plaintext or ciphertext. `upstream_api_key` is the one secret
    /// in this schema that cannot be reduced to a hash (the proxy has to
    /// present it upstream), so the only safe thing to say about it here is
    /// whether it is set.
    has_upstream_api_key: bool,
}

#[derive(Serialize)]
struct ModelView {
    id: i64,
    name: String,
    description: String,
    backends: Vec<BackendView>,
}

async fn list_models(State(ctx): State<Ctx>) -> Result<Json<Vec<ModelView>>, ApiError> {
    let models: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, description FROM models ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing models", &e))?;
    // `upstream_api_key IS NOT NULL` rather than the column: this query must
    // not be able to return a credential even by accident, so the ciphertext
    // never leaves Postgres on this path at all.
    let backends: Vec<(i64, i64, String, String, bool)> = sqlx::query_as(
        "SELECT id, model_id, api_base, upstream_model, upstream_api_key IS NOT NULL
         FROM model_backends ORDER BY id",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing backends", &e))?;

    Ok(Json(
        models
            .into_iter()
            .map(|(id, name, description)| ModelView {
                id,
                name,
                description,
                backends: backends
                    .iter()
                    .filter(|(_, model_id, ..)| *model_id == id)
                    .map(|(bid, _, api_base, upstream_model, has_key)| BackendView {
                        id: *bid,
                        api_base: api_base.clone(),
                        upstream_model: upstream_model.clone(),
                        has_upstream_api_key: *has_key,
                    })
                    .collect(),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewModel {
    name: String,
    #[serde(default)]
    description: String,
}

async fn post_model(
    State(ctx): State<Ctx>,
    Json(body): Json<NewModel>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must not be empty; it is the name clients address this model by",
        ));
    }
    let id: i64 =
        sqlx::query_scalar("INSERT INTO models (name, description) VALUES ($1, $2) RETURNING id")
            .bind(&body.name)
            .bind(&body.description)
            .fetch_one(&ctx.pool)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    api_error(
                        StatusCode::CONFLICT,
                        format!(
                            "a model named {:?} already exists; add another backend to it with \
                             POST /admin/models/{{id}}/backends instead of creating a second model",
                            body.name
                        ),
                    )
                } else {
                    db_error("model creation", &e)
                }
            })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": body.name })),
    ))
}

async fn delete_model(State(ctx): State<Ctx>, Path(id): Path<i64>) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM models WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("model deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no model with id {id}; GET /admin/models lists the ids that exist"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NewBackend {
    api_base: String,
    /// Absent means "the model's own name", matching what `File` mode does
    /// with a `litellm_params` entry that names no `model`.
    upstream_model: Option<String>,
    /// Encrypted before it reaches Postgres and never readable back through
    /// this API — `GET /admin/models` reports only whether one is set.
    upstream_api_key: Option<String>,
}

async fn post_backend(
    State(ctx): State<Ctx>,
    Path(model_id): Path<i64>,
    Json(body): Json<NewBackend>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let api_base = body.api_base.trim().trim_end_matches('/').to_string();
    if !(api_base.starts_with("http://") || api_base.starts_with("https://")) {
        // Same rule `FileConfig::validate` applies, and for the same reason:
        // accepting the URL and then failing every request against it is the
        // worst of both.
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("api_base {api_base:?} must start with http:// or https://"),
        ));
    }
    // Fetched rather than assumed so a bad `model_id` is reported as such,
    // and so `upstream_model` can default to the model's own name.
    let model_name: Option<String> = sqlx::query_scalar("SELECT name FROM models WHERE id = $1")
        .bind(model_id)
        .fetch_optional(&ctx.pool)
        .await
        .map_err(|e| db_error("model lookup", &e))?;
    let Some(model_name) = model_name else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("no model with id {model_id}; GET /admin/models lists the ids that exist"),
        ));
    };
    let upstream_model = body
        .upstream_model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&model_name)
        .to_string();

    let encrypted = body
        .upstream_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| crate::control::secrets::encrypt(&ctx.key, k))
        .transpose()
        .map_err(|e| {
            tracing::error!(error = %e, "encrypting upstream_api_key failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not encrypt upstream_api_key; the backend was not created",
            )
        })?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(model_id)
    .bind(&api_base)
    .bind(&upstream_model)
    .bind(encrypted)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| db_error("backend creation", &e))?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id,
            "model_id": model_id,
            "api_base": api_base,
            "upstream_model": upstream_model,
        })),
    ))
}

async fn delete_backend(
    State(ctx): State<Ctx>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM model_backends WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("backend deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no backend with id {id}; GET /admin/models lists each model's backend ids"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct PermissionView {
    verb: String,
    resource: String,
}

#[derive(Serialize)]
struct RoleView {
    id: i64,
    name: String,
    description: String,
    permissions: Vec<PermissionView>,
}

async fn list_roles(State(ctx): State<Ctx>) -> Result<Json<Vec<RoleView>>, ApiError> {
    let roles: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, description FROM roles ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing roles", &e))?;
    let perms: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT rp.role_id, p.verb, p.resource FROM permissions p
         JOIN role_permissions rp ON rp.permission_id = p.id
         ORDER BY p.verb, p.resource",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing role permissions", &e))?;

    Ok(Json(
        roles
            .into_iter()
            .map(|(id, name, description)| RoleView {
                id,
                name,
                description,
                permissions: perms
                    .iter()
                    .filter(|(role_id, ..)| *role_id == id)
                    .map(|(_, verb, resource)| PermissionView {
                        verb: verb.clone(),
                        resource: resource.clone(),
                    })
                    .collect(),
            })
            .collect(),
    ))
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
/// separate process, and still the documented way to seed or re-seed from a
/// config file), or a hand-written `UPDATE`/`INSERT` against Postgres, which
/// `deploy/README.md` no longer recommends now that every model, backend,
/// principal and role change has a route, but which remains reachable and
/// must not silently do nothing. Before this existed,
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
    // Every mutating route below ends in `refresh(&ctx)` — the one write
    // path that publishes through `SnapshotSink::store_snapshot`. That is
    // what keeps the published snapshot and (in `--role all`) the routing
    // `Registry` from ever disagreeing about what was just changed; adding a
    // route that writes a row without it would reintroduce exactly the
    // "changed in Postgres, invisible to a running process" gap that
    // `spawn_snapshot_rebuilder` exists to paper over.
    let app = Router::new()
        .route("/admin/keys", get(list_keys).post(post_key))
        .route("/admin/keys/{id}", delete(revoke_key))
        .route(
            "/admin/principals",
            get(list_principals).post(post_principal),
        )
        .route("/admin/principals/{id}", delete(delete_principal))
        .route("/admin/principals/{id}/roles", post(grant_role))
        .route("/admin/principals/{id}/roles/{role}", delete(revoke_role))
        .route("/admin/models", get(list_models).post(post_model))
        .route("/admin/models/{id}", delete(delete_model))
        .route("/admin/models/{id}/backends", post(post_backend))
        .route("/admin/backends/{id}", delete(delete_backend))
        .route("/admin/roles", get(list_roles))
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

    fn unique_name(tag: &str) -> String {
        format!(
            "{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    async fn test_ctx() -> (Ctx, Arc<dyn SnapshotSink>) {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        let cache: Arc<dyn SnapshotSink> = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
        let ctx = Ctx {
            pool,
            proxy_token: "test-token".into(),
            cache: Arc::clone(&cache),
            key: Arc::new(test_key()),
        };
        (ctx, cache)
    }

    #[test]
    fn a_short_key_gets_no_display_prefix_at_all() {
        // `sk-eval` from a hand-written `auth:` block: 11 characters of it
        // would be all of it, and `GET /admin/keys` hands prefixes back.
        assert_eq!(safe_display_prefix("sk-eval"), "(hidden)");
        let minted = generate_key();
        assert_eq!(safe_display_prefix(&minted), display_prefix(&minted));
    }

    /// The load-bearing invariant of this API: every mutating route lands in
    /// the published snapshot, because every one of them goes through the
    /// same `refresh` -> `SnapshotSink::store_snapshot` path the key routes
    /// have always used. A route that wrote a row and skipped it would be
    /// invisible to a running proxy until the periodic rebuilder happened to
    /// notice.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn every_mutating_route_publishes_through_the_one_write_path() {
        let (ctx, cache) = test_ctx().await;
        let principal_name = unique_name("route-principal");
        let model_name = unique_name("route-model");

        let (status, created) = post_principal(
            State(ctx.clone()),
            Json(NewPrincipal {
                name: principal_name.clone(),
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        let principal_id = created.0["id"].as_i64().unwrap();

        grant_role(
            State(ctx.clone()),
            Path(principal_id),
            Json(RoleGrant {
                role: "inference".into(),
            }),
        )
        .await
        .unwrap();

        let (_, model) = post_model(
            State(ctx.clone()),
            Json(NewModel {
                name: model_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        let model_id = model.0["id"].as_i64().unwrap();

        let upstream_credential = "sk-upstream-must-never-come-back";
        let (_, backend) = post_backend(
            State(ctx.clone()),
            Path(model_id),
            Json(NewBackend {
                api_base: "http://route-test:8000/v1/".into(),
                upstream_model: None,
                upstream_api_key: Some(upstream_credential.into()),
            }),
        )
        .await
        .unwrap();
        let backend_id = backend.0["id"].as_i64().unwrap();

        // The snapshot the routes published, not one this test built.
        let snap = cache.current_snapshot();
        let published = snap
            .models
            .iter()
            .find(|m| m.name == model_name)
            .expect("a model created over the admin API must be in the published snapshot");
        assert_eq!(published.backends.len(), 1);
        // The trailing slash is stripped on write, same as `import` does.
        assert_eq!(published.backends[0].api_base, "http://route-test:8000/v1");
        assert_eq!(published.backends[0].upstream_model, model_name);
        // Encrypted on write, decrypted by `build_snapshot`: the proxy has
        // to present this upstream, so the snapshot carries it usably.
        assert_eq!(
            published.backends[0].api_key.as_deref(),
            Some(upstream_credential)
        );
        assert!(snap
            .principals
            .values()
            .any(|p| p.name == principal_name && p.allow_all));

        // The credential must not come back out of the read route, in any form.
        let listed = list_models(State(ctx.clone())).await.unwrap();
        let json = serde_json::to_string(&listed.0).unwrap();
        assert!(!json.contains(upstream_credential));
        assert!(json.contains("has_upstream_api_key"));

        // Encrypted at rest, not merely absent from the response.
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT upstream_api_key FROM model_backends WHERE id = $1")
                .bind(backend_id)
                .fetch_one(&ctx.pool)
                .await
                .unwrap();
        assert_ne!(stored, upstream_credential.as_bytes());

        revoke_role(
            State(ctx.clone()),
            Path((principal_id, "inference".to_string())),
        )
        .await
        .unwrap();
        assert!(
            !cache
                .current_snapshot()
                .principals
                .values()
                .any(|p| p.name == principal_name && p.allow_all),
            "a revoked role must reach the published snapshot too"
        );

        delete_backend(State(ctx.clone()), Path(backend_id))
            .await
            .unwrap();
        delete_model(State(ctx.clone()), Path(model_id))
            .await
            .unwrap();
        delete_principal(State(ctx.clone()), Path(principal_id))
            .await
            .unwrap();
        assert!(
            !cache
                .current_snapshot()
                .models
                .iter()
                .any(|m| m.name == model_name),
            "a deleted model must leave the published snapshot"
        );
    }

    /// `GET /admin/keys` exists to identify keys, not to hand them out. The
    /// hash is the verifier and has no business in a response body, and the
    /// prefix must stay a prefix.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn listing_keys_returns_no_key_material() {
        let (ctx, _cache) = test_ctx().await;
        let principal_name = unique_name("key-listing-principal");
        let (_, created) = post_principal(
            State(ctx.clone()),
            Json(NewPrincipal {
                name: principal_name.clone(),
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        let principal_id = created.0["id"].as_i64().unwrap();

        let key_name = unique_name("listed-key");
        let (plaintext, id) = create_key(&ctx.pool, &key_name, principal_id, None)
            .await
            .unwrap();

        let listed = list_keys(State(ctx.clone())).await.unwrap();
        let mine = listed.0.iter().find(|k| k.id == id).unwrap();
        assert_eq!(mine.principal, principal_name);
        assert_eq!(mine.name, key_name);
        assert!(!mine.disabled);

        let json = serde_json::to_string(&listed.0).unwrap();
        assert!(!json.contains(&plaintext), "the plaintext key was returned");
        assert!(
            !json.contains(&hex::encode(hash_key(&plaintext))),
            "the key hash was returned"
        );
        assert!(!json.contains("hash"));
        assert!(
            plaintext.starts_with(&mine.prefix) && mine.prefix.len() < plaintext.len(),
            "prefix must identify the key without being it"
        );
    }

    /// Same standard `post_key` was held to: a bad id has to say which id and
    /// what was wrong with it, because `/admin/*` has no UI and the error
    /// body is the whole diagnostic.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn bad_ids_and_names_are_reported_descriptively() {
        let (ctx, _cache) = test_ctx().await;

        for (status, body) in [
            delete_model(State(ctx.clone()), Path(-1))
                .await
                .unwrap_err(),
            delete_backend(State(ctx.clone()), Path(-1))
                .await
                .unwrap_err(),
            delete_principal(State(ctx.clone()), Path(-1))
                .await
                .unwrap_err(),
            revoke_key(State(ctx.clone()), Path(-1)).await.unwrap_err(),
        ] {
            assert_eq!(status, StatusCode::NOT_FOUND);
            let message = body.0["error"].as_str().unwrap();
            assert!(message.contains("-1"), "must name the id: {message}");
        }

        let (status, body) = grant_role(
            State(ctx.clone()),
            Path(1),
            Json(RoleGrant {
                role: "not-a-role".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body.0["error"].as_str().unwrap();
        assert!(
            message.contains("not-a-role") && message.contains("inference"),
            "an unknown role must name the roles that do exist: {message}"
        );

        let (status, body) = post_backend(
            State(ctx.clone()),
            Path(-1),
            Json(NewBackend {
                api_base: "http://x:8000/v1".into(),
                upstream_model: None,
                upstream_api_key: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("-1"));

        let (status, _) = post_principal(
            State(ctx.clone()),
            Json(NewPrincipal {
                name: unique_name("bad-kind"),
                kind: Some("robot".into()),
                email: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// `GET /admin/roles` is what makes `POST /admin/principals/{id}/roles`
    /// usable without reading the schema: it has to show what each role
    /// actually grants, not just its name.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn roles_are_listed_with_their_permissions() {
        let (ctx, _cache) = test_ctx().await;
        let roles = list_roles(State(ctx)).await.unwrap();
        let inference = roles.0.iter().find(|r| r.name == "inference").unwrap();
        assert!(inference
            .permissions
            .iter()
            .any(|p| p.verb == "model:invoke" && p.resource == "model/*"));
        let admin = roles.0.iter().find(|r| r.name == "admin").unwrap();
        assert!(admin.permissions.len() > inference.permissions.len());
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
