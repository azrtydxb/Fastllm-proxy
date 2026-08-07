//! Admin API and the snapshot endpoint.
//!
//! `/snapshot` is read-only and authenticated with the proxy's own token,
//! which is distinct from any user key: a stolen proxy token discloses policy
//! — key hashes, never plaintext — and grants nothing else.

use crate::control::build::build_snapshot;
use crate::control::reconcile::ReconcileState;
use crate::control::secrets::EncryptionKey;
use crate::routing::MatchConditionJson;
use crate::snapshot::{constant_time_eq, hash_key, Snapshot};
use crate::usage::UsageEvent;
use crate::vector::cosine;
use arc_swap::ArcSwap;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// How many times `refresh` has run `build_snapshot` after a write and
    /// had it fail. The write itself already committed by the time `refresh`
    /// runs — see `refresh`'s doc comment for why a failure here is
    /// deliberately not turned into a 5xx on the route that triggered it —
    /// so this counter, surfaced on `GET /admin/health`, is what makes that
    /// otherwise-silent divergence between "the database" and "the published
    /// snapshot" something an operator can actually notice.
    snapshot_rebuild_failures: Arc<AtomicU64>,
    /// P2 reconciliation's server-side aggregation state — see
    /// `control::reconcile`'s doc comment for why this is in-memory only,
    /// not a table. One instance for the life of the process, shared by
    /// every `POST /limits/reconcile` call the same way `pool` is shared by
    /// every database query.
    reconcile: Arc<ReconcileState>,
    /// Whether `serve` bound with TLS — the one bit `login`/`logout` need to
    /// decide whether to set `Secure` on the session cookie. `Secure`
    /// unconditionally would make the cookie silently never round-trip over
    /// the plain-HTTP dev path this crate already tolerates for `/snapshot`;
    /// omitting it whenever TLS *is* on would ship a session cookie plain
    /// HTTP could read.
    tls_enabled: bool,
}

/// Every admin route fails the same way `post_key` does: a status plus a JSON
/// body that says what was wrong. `/admin/*` is authenticated (`require_session`)
/// and authorised (`RequirePermission`) and has a management UI (`control::ui`),
/// but none of that changes the diagnostic an operator gets on failure — a bare
/// status code would still just mean reading the SQL to find out which id was
/// the typo.
type ApiError = (StatusCode, Json<serde_json::Value>);

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// For failures the caller could not have caused and cannot act on. The
/// structured `sqlx::Error` goes to the log, not to the response: it can name
/// columns and constraints, and an authenticated, authorised operator still
/// has no legitimate use for that level of internal detail over the wire.
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
    _perm: RequireKeyCreate,
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

async fn revoke_key(
    State(ctx): State<Ctx>,
    _perm: RequireKeyRevoke,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
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

async fn list_keys(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<KeyView>>, ApiError> {
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

async fn list_principals(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<PrincipalView>>, ApiError> {
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
    _perm: RequireConfigWrite,
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
    _perm: RequireConfigWrite,
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
    _perm: RequireConfigWrite,
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
    _perm: RequireConfigWrite,
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
    /// Which wire format this backend speaks — `openai` for the OpenAI-
    /// compatible majority, `anthropic` or `gemini` for a natively translated
    /// one. Surfaced because it changes which request features are available
    /// (see `crate::protocol`), so an operator debugging a 501 can see it
    /// without reading the database.
    protocol: String,
    auth_header: String,
    default_max_tokens: Option<i32>,
}

#[derive(Serialize)]
struct ModelView {
    id: i64,
    name: String,
    description: String,
    backends: Vec<BackendView>,
}

async fn list_models(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<ModelView>>, ApiError> {
    let models: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, description FROM models ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing models", &e))?;
    // `upstream_api_key IS NOT NULL` rather than the column: this query must
    // not be able to return a credential even by accident, so the ciphertext
    // never leaves Postgres on this path at all.
    type BackendListRow = (i64, i64, String, String, bool, String, String, Option<i32>);
    let backends: Vec<BackendListRow> = sqlx::query_as(
        "SELECT id, model_id, api_base, upstream_model, upstream_api_key IS NOT NULL, \
         protocol, auth_header, default_max_tokens
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
                    .map(
                        |(
                            bid,
                            _,
                            api_base,
                            upstream_model,
                            has_key,
                            protocol,
                            auth_header,
                            default_max_tokens,
                        )| BackendView {
                            id: *bid,
                            api_base: api_base.clone(),
                            upstream_model: upstream_model.clone(),
                            has_upstream_api_key: *has_key,
                            protocol: protocol.clone(),
                            auth_header: auth_header.clone(),
                            default_max_tokens: *default_max_tokens,
                        },
                    )
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

/// A model and a virtual model sharing a name would make the `model` field
/// in a request body ambiguous about which one a client meant — rather than
/// pick a silent precedence order between them, creation of either is
/// refused while the other name is taken. See `post_virtual_model`'s mirror
/// check.
async fn virtual_model_name_exists(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM virtual_models WHERE name = $1)")
        .bind(name)
        .fetch_one(pool)
        .await
}

async fn model_name_exists(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM models WHERE name = $1)")
        .bind(name)
        .fetch_one(pool)
        .await
}

async fn post_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewModel>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must not be empty; it is the name clients address this model by",
        ));
    }
    if virtual_model_name_exists(&ctx.pool, &body.name)
        .await
        .map_err(|e| db_error("model creation", &e))?
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "a virtual model named {:?} already exists; a model and a virtual model cannot \
                 share a name, since a client request naming it would be ambiguous",
                body.name
            ),
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

async fn delete_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
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
    /// Everything below defaults to today's behaviour, so a caller (or a
    /// test) that names none of them gets an OpenAI-compatible backend
    /// reached with `Authorization: Bearer`, exactly as before.
    /// Wire format this upstream speaks. Absent means `openai`, which covers
    /// every OpenAI-compatible provider — vLLM, OpenRouter, Groq, Together,
    /// DeepSeek and the rest. Only `anthropic` and `gemini` need saying.
    #[serde(default)]
    protocol: Option<String>,
    /// Header the key is sent in, and the prefix before it. Both default to
    /// `Authorization: Bearer`; the two native protocols set them
    /// automatically, so an operator normally leaves both unset.
    #[serde(default)]
    auth_header: Option<String>,
    #[serde(default)]
    auth_scheme: Option<String>,
    /// Supplies `max_tokens` when a request omits one and the provider
    /// requires it (Anthropic does). Left unset, such a request is refused
    /// with a message naming this field — deliberately, rather than being
    /// capped at a number nobody chose.
    #[serde(default)]
    default_max_tokens: Option<i32>,
    /// How to read `upstream_api_key`. Absent means `static` — the key is the
    /// credential. `gcp_service_account` means it is a Google service-account
    /// key file, which the control plane exchanges for an access token on
    /// every snapshot build; that is what Vertex AI needs, and the only
    /// provider here that cannot use a static secret.
    #[serde(default)]
    credential_kind: Option<String>,
}

impl NewBackend {
    /// An OpenAI-compatible backend with nothing special about it — what the
    /// unit tests below construct, and the shape every field here defaults to.
    #[cfg(test)]
    fn openai(api_base: &str, upstream_api_key: Option<String>) -> Self {
        Self {
            api_base: api_base.into(),
            upstream_model: None,
            upstream_api_key,
            protocol: None,
            auth_header: None,
            auth_scheme: None,
            default_max_tokens: None,
            credential_kind: None,
        }
    }
}

/// Fill in the auth defaults a protocol implies, so an operator adding an
/// Anthropic backend does not have to know that it wants a raw key in
/// `x-api-key` rather than a bearer token — and cannot get it wrong.
fn auth_defaults_for(protocol: &str) -> (&'static str, Option<&'static str>) {
    match protocol {
        "anthropic" => ("x-api-key", None),
        "gemini" => ("x-goog-api-key", None),
        _ => ("authorization", Some("Bearer")),
    }
}

async fn post_backend(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
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

    let protocol = body.protocol.unwrap_or_else(|| "openai".into());
    // Rejected here rather than left to the column's CHECK constraint so the
    // caller gets the list of valid values instead of a Postgres error, and
    // before an unusable row exists.
    if crate::protocol::Protocol::parse(&protocol).is_none() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("protocol {protocol:?} is not one of: openai, anthropic, gemini"),
        ));
    }
    if body.default_max_tokens.is_some_and(|n| n <= 0) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "default_max_tokens must be greater than zero".to_string(),
        ));
    }
    let credential_kind = body.credential_kind.unwrap_or_else(|| "static".into());
    if !matches!(credential_kind.as_str(), "static" | "gcp_service_account") {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "credential_kind {credential_kind:?} is not one of: static, gcp_service_account"
            ),
        ));
    }
    // Checked at write time because the alternative is a backend that looks
    // configured, disappears from routing on the next rebuild, and explains
    // itself only in the control plane's log.
    if credential_kind == "gcp_service_account"
        && !body
            .upstream_api_key
            .as_deref()
            .is_some_and(crate::control::gcp::ServiceAccount::looks_like_one)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "credential_kind gcp_service_account needs upstream_api_key to be the service              account's JSON key file, with `client_email` and `private_key`"
                .to_string(),
        ));
    }
    let (default_header, default_scheme) = auth_defaults_for(&protocol);
    let auth_header = body
        .auth_header
        .unwrap_or_else(|| default_header.to_string());
    // An explicitly empty scheme means "send the raw key"; only an absent one
    // falls back to the protocol's default.
    let auth_scheme = match body.auth_scheme {
        Some(s) if s.trim().is_empty() => None,
        Some(s) => Some(s),
        None => default_scheme.map(str::to_string),
    };

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key, \
         protocol, auth_header, auth_scheme, default_max_tokens, credential_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
    )
    .bind(model_id)
    .bind(&api_base)
    .bind(&upstream_model)
    .bind(encrypted)
    .bind(&protocol)
    .bind(&auth_header)
    .bind(&auth_scheme)
    .bind(body.default_max_tokens)
    .bind(&credential_kind)
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
            "protocol": protocol,
            "auth_header": auth_header,
            "auth_scheme": auth_scheme,
            "default_max_tokens": body.default_max_tokens,
            "credential_kind": credential_kind,
        })),
    ))
}

async fn delete_backend(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
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

// --- Virtual models, routing rules and their targets ------------------
//
// Same CRUD shape as models/backends above: one route per table, every
// mutating one ending in `refresh(&ctx)` so a write reaches the published
// snapshot through the one path every other admin route uses (see
// `refresh`'s doc comment). `crate::routing`/`control::build::build_virtual_models`
// is what actually resolves these four tables into the pre-evaluated form
// the request path reads; the routes here only ever write rows.

#[derive(Serialize)]
struct TargetView {
    id: i64,
    model_id: i64,
    model: String,
    weight: i32,
    position: i32,
}

#[derive(Serialize)]
struct RuleView {
    id: i64,
    position: i32,
    #[serde(flatten)]
    match_condition: MatchConditionJson,
    targets: Vec<TargetView>,
}

#[derive(Serialize)]
struct VirtualModelView {
    id: i64,
    name: String,
    description: String,
    rules: Vec<RuleView>,
    /// Used when no rule matches; see the migration's comment on
    /// `virtual_model_defaults` for why this is its own table rather than an
    /// always-true rule.
    default_targets: Vec<TargetView>,
}

// (id, owner_id, model_id, model_name, weight, position) — `owner_id` is
// `rule_id` for a rule's own targets and `virtual_model_id` for a virtual
// model's defaults; the two queries below share this shape so `to_targets`
// works for either without duplicating it.
type TargetRow = (i64, i64, i64, String, i32, i32);

async fn list_virtual_models(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<VirtualModelView>>, ApiError> {
    let vms: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, description FROM virtual_models ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing virtual models", &e))?;
    let rules: Vec<(i64, i64, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT id, virtual_model_id, position, match_json FROM routing_rules
         ORDER BY virtual_model_id, position",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing routing rules", &e))?;
    let rule_targets: Vec<TargetRow> = sqlx::query_as(
        "SELECT rt.id, rt.rule_id, rt.model_id, m.name, rt.weight, rt.position
         FROM rule_targets rt JOIN models m ON m.id = rt.model_id
         ORDER BY rt.rule_id, rt.position",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing rule targets", &e))?;
    let default_targets: Vec<TargetRow> = sqlx::query_as(
        "SELECT vd.id, vd.virtual_model_id, vd.model_id, m.name, vd.weight, vd.position
         FROM virtual_model_defaults vd JOIN models m ON m.id = vd.model_id
         ORDER BY vd.virtual_model_id, vd.position",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing virtual model defaults", &e))?;

    let to_targets = |owner: i64, rows: &[TargetRow]| -> Vec<TargetView> {
        rows.iter()
            .filter(|(_, o, ..)| *o == owner)
            .map(|(id, _, model_id, model, weight, position)| TargetView {
                id: *id,
                model_id: *model_id,
                model: model.clone(),
                weight: *weight,
                position: *position,
            })
            .collect()
    };

    Ok(Json(
        vms.into_iter()
            .map(|(vm_id, name, description)| {
                let rule_views = rules
                    .iter()
                    .filter(|(_, virtual_model_id, ..)| *virtual_model_id == vm_id)
                    .map(|(rule_id, _, position, match_json)| {
                        // A rule whose `match_json` cannot parse is still
                        // listed rather than hidden — an operator diagnosing
                        // why `build_snapshot` dropped it (see that
                        // function's doc comment) needs to see it exists,
                        // not have it vanish from both the database view
                        // and the runtime snapshot.
                        let match_condition: MatchConditionJson =
                            serde_json::from_value(match_json.clone()).unwrap_or_default();
                        RuleView {
                            id: *rule_id,
                            position: *position,
                            match_condition,
                            targets: to_targets(*rule_id, &rule_targets),
                        }
                    })
                    .collect();
                VirtualModelView {
                    id: vm_id,
                    name,
                    description,
                    rules: rule_views,
                    default_targets: to_targets(vm_id, &default_targets),
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewVirtualModel {
    name: String,
    #[serde(default)]
    description: String,
}

async fn post_virtual_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewVirtualModel>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must not be empty; it is the name clients address this virtual model by",
        ));
    }
    if model_name_exists(&ctx.pool, &body.name)
        .await
        .map_err(|e| db_error("virtual model creation", &e))?
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "a model named {:?} already exists; a model and a virtual model cannot share a \
                 name, since a client request naming it would be ambiguous",
                body.name
            ),
        ));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO virtual_models (name, description) VALUES ($1, $2) RETURNING id",
    )
    .bind(&body.name)
    .bind(&body.description)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!("a virtual model named {:?} already exists", body.name),
            )
        } else {
            db_error("virtual model creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": body.name })),
    ))
}

async fn delete_virtual_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // ON DELETE CASCADE (migrations/0008) takes this virtual model's rules
    // and defaults with it, same reasoning as a principal's keys/grants.
    let done = sqlx::query("DELETE FROM virtual_models WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("virtual model deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!(
                "no virtual model with id {id}; GET /admin/virtual-models lists the ids that exist"
            ),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NewRule {
    /// Where this rule sits in its virtual model's evaluation order —
    /// load-bearing, not cosmetic: the first matching rule wins.
    position: i32,
    #[serde(flatten)]
    match_condition: MatchConditionJson,
}

async fn post_rule(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(virtual_model_id): Path<i64>,
    Json(body): Json<NewRule>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Validated at write time so a malformed `"25:00"` or a `days: [8]` is a
    // 400 the operator sees immediately, rather than a rule that parses,
    // stores, and then silently never matches — the failure this repo keeps
    // catching by review instead of by test.
    crate::routing::validate_match_json(&body.match_condition)
        .map_err(|why| api_error(StatusCode::BAD_REQUEST, why))?;
    let match_json = serde_json::to_value(&body.match_condition)
        .expect("MatchConditionJson has no non-serialisable field");
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO routing_rules (virtual_model_id, position, match_json)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(virtual_model_id)
    .bind(body.position)
    .bind(match_json)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "no virtual model with id {virtual_model_id}; GET /admin/virtual-models \
                     lists the ids that exist"
                ),
            )
        } else if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "virtual model {virtual_model_id} already has a rule at position {}",
                    body.position
                ),
            )
        } else {
            db_error("routing rule creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(
            serde_json::json!({ "id": id, "virtual_model_id": virtual_model_id, "position": body.position }),
        ),
    ))
}

async fn delete_rule(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM routing_rules WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("routing rule deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no rule with id {id}; GET /admin/virtual-models lists each rule's id"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NewTarget {
    model_id: i64,
    #[serde(default = "default_target_weight")]
    weight: i32,
    /// Failover order within the chain — see
    /// `crate::routing::VirtualModelDef::resolve`'s doc comment.
    position: i32,
}

/// A single target with no sibling has nothing to split against, so a
/// sensible default lets the common "one rule, one target" case omit
/// `weight` entirely rather than requiring a caller to always write `100`.
fn default_target_weight() -> i32 {
    100
}

async fn post_rule_target(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(rule_id): Path<i64>,
    Json(body): Json<NewTarget>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO rule_targets (rule_id, model_id, weight, position)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(rule_id)
    .bind(body.model_id)
    .bind(body.weight)
    .bind(body.position)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "no rule with id {rule_id} or no model with id {}; a target needs both to exist",
                    body.model_id
                ),
            )
        } else if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "rule {rule_id} already has a target at position {}",
                    body.position
                ),
            )
        } else {
            db_error("rule target creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

async fn delete_rule_target(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM rule_targets WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("rule target deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no rule target with id {id}; GET /admin/virtual-models lists each one's id"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_default_target(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(virtual_model_id): Path<i64>,
    Json(body): Json<NewTarget>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO virtual_model_defaults (virtual_model_id, model_id, weight, position)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(virtual_model_id)
    .bind(body.model_id)
    .bind(body.weight)
    .bind(body.position)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "no virtual model with id {virtual_model_id} or no model with id {}; a \
                     default target needs both to exist",
                    body.model_id
                ),
            )
        } else if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "virtual model {virtual_model_id} already has a default target at position {}",
                    body.position
                ),
            )
        } else {
            db_error("default target creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

async fn delete_default_target(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM virtual_model_defaults WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("default target deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!(
                "no default target with id {id}; GET /admin/virtual-models lists each one's id"
            ),
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

async fn list_roles(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<RoleView>>, ApiError> {
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

// --- Rate limits (P2) --------------------------------------------------
//
// Same shape as every other admin route in this file: read the current
// state, write a row, `refresh(&ctx)` so the write reaches the published
// snapshot. See `migrations/0009_limits.sql` for the schema and
// `crate::control::build::build_snapshot` for how a row here becomes
// `Principal::limits`.

#[derive(Serialize)]
struct LimitView {
    principal_id: i64,
    principal: String,
    requests_per_min: Option<i32>,
    tokens_per_min: Option<i32>,
}

async fn list_limits(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<LimitView>>, ApiError> {
    let rows: Vec<(i64, String, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT l.principal_id, p.name, l.requests_per_min, l.tokens_per_min
         FROM limits l JOIN principals p ON p.id = l.principal_id
         ORDER BY l.principal_id",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing limits", &e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(principal_id, principal, requests_per_min, tokens_per_min)| LimitView {
                    principal_id,
                    principal,
                    requests_per_min,
                    tokens_per_min,
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PutLimits {
    #[serde(default)]
    requests_per_min: Option<i32>,
    #[serde(default)]
    tokens_per_min: Option<i32>,
}

/// `PUT`, not `POST`: this is an upsert on the principal, matching the
/// schema's one-row-per-principal shape (`migrations/0009_limits.sql`'s
/// `PRIMARY KEY (principal_id)`) — calling it again with a new value updates
/// the existing row rather than erroring or creating a second one.
async fn put_limits(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(principal_id): Path<i64>,
    Json(body): Json<PutLimits>,
) -> Result<StatusCode, ApiError> {
    if body.requests_per_min.is_none() && body.tokens_per_min.is_none() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "at least one of requests_per_min/tokens_per_min must be set; \
             DELETE /admin/principals/{id}/limits removes a limit instead",
        ));
    }
    for (field, value) in [
        ("requests_per_min", body.requests_per_min),
        ("tokens_per_min", body.tokens_per_min),
    ] {
        if value.is_some_and(|v| v <= 0) {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("{field} must be a positive number of units per minute"),
            ));
        }
    }
    sqlx::query(
        "INSERT INTO limits (principal_id, requests_per_min, tokens_per_min)
         VALUES ($1, $2, $3)
         ON CONFLICT (principal_id)
         DO UPDATE SET requests_per_min = EXCLUDED.requests_per_min,
                        tokens_per_min = EXCLUDED.tokens_per_min,
                        updated_at = now()",
    )
    .bind(principal_id)
    .bind(body.requests_per_min)
    .bind(body.tokens_per_min)
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
            db_error("limit upsert", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_limits(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(principal_id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM limits WHERE principal_id = $1")
        .bind(principal_id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("limit deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("principal {principal_id} has no configured limit to remove"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

// --- Budgets (P3) -------------------------------------------------------
//
// Same shape as `limits` just above: read the current state, upsert a row,
// `refresh(&ctx)` so the write reaches the published snapshot. See
// `migrations/0010_budgets.sql` for the schema and
// `crate::control::build::roll_over_and_load_budgets` for how a row here
// becomes `Principal.budget` (including window rollover, which runs on
// every snapshot rebuild, not just here).

const BUDGET_WINDOWS: [&str; 3] = ["daily", "weekly", "monthly"];

#[derive(Serialize)]
struct BudgetView {
    principal_id: i64,
    principal: String,
    tokens_total: i64,
    tokens_used: i64,
    window: String,
    window_start: chrono::DateTime<chrono::Utc>,
}

async fn list_budgets(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<BudgetView>>, ApiError> {
    type BudgetRow = (i64, String, i64, i64, String, chrono::DateTime<chrono::Utc>);
    let rows: Vec<BudgetRow> = sqlx::query_as(
        "SELECT b.principal_id, p.name, b.tokens_total, b.tokens_used, b.budget_window, b.window_start
         FROM budgets b JOIN principals p ON p.id = b.principal_id
         ORDER BY b.principal_id",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing budgets", &e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(principal_id, principal, tokens_total, tokens_used, window, window_start)| {
                    BudgetView {
                        principal_id,
                        principal,
                        tokens_total,
                        tokens_used,
                        window,
                        window_start,
                    }
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
struct PutBudget {
    tokens_total: i64,
    window: String,
}

/// `PUT`, not `POST`: same upsert-on-the-principal shape `put_limits` uses,
/// matching `budgets`' one-row-per-principal primary key. Deliberately
/// leaves `tokens_used`/`window_start` alone on an update — raising or
/// lowering `tokens_total` (e.g. after a billing conversation) must not
/// incidentally reset a principal's consumption or restart its window; that
/// is what `DELETE` followed by a fresh `PUT`, or waiting for the window to
/// roll over on its own, are for.
async fn put_budget(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(principal_id): Path<i64>,
    Json(body): Json<PutBudget>,
) -> Result<StatusCode, ApiError> {
    if body.tokens_total <= 0 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "tokens_total must be a positive number of tokens",
        ));
    }
    if !BUDGET_WINDOWS.contains(&body.window.as_str()) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "window {:?} is not one of {}",
                body.window,
                BUDGET_WINDOWS.join(", ")
            ),
        ));
    }
    sqlx::query(
        "INSERT INTO budgets (principal_id, tokens_total, budget_window)
         VALUES ($1, $2, $3)
         ON CONFLICT (principal_id)
         DO UPDATE SET tokens_total = EXCLUDED.tokens_total, budget_window = EXCLUDED.budget_window,
                        updated_at = now()",
    )
    .bind(principal_id)
    .bind(body.tokens_total)
    .bind(&body.window)
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
            db_error("budget upsert", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_budget(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(principal_id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM budgets WHERE principal_id = $1")
        .bind(principal_id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("budget deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("principal {principal_id} has no configured budget to remove"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ReconcileCountWire {
    principal_id: u64,
    requests: u64,
    tokens: u64,
}

#[derive(Deserialize)]
struct ReconcileRequest {
    replica_id: String,
    counts: Vec<ReconcileCountWire>,
}

#[derive(Serialize)]
struct AllowanceWire {
    principal_id: u64,
    requests_share: f64,
    tokens_share: f64,
}

#[derive(Serialize)]
struct ReconcileResponse {
    allowances: Vec<AllowanceWire>,
}

/// `POST /limits/reconcile`: the P2 half of the Snapshot protocol's reverse
/// channel, sibling to `/usage` and gated by the same proxy bootstrap token
/// (a stolen token already discloses every key hash and usable backend
/// credential via `/snapshot`; letting it also exchange rate-limit counts
/// grants nothing new). Unlike `/usage` this is not fire-and-forget: the
/// whole point is the allowance in the response body — see
/// `crate::control::reconcile::ReconcileState::report` for the aggregation
/// itself, which is a pure function tested independently of this wire
/// wrapper.
async fn post_reconcile(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Json(body): Json<ReconcileRequest>,
) -> Result<Json<ReconcileResponse>, ApiError> {
    if !proxy_token_authorised(&headers, &ctx.proxy_token) {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "bad or missing proxy token",
        ));
    }
    let now = std::time::Instant::now();
    let allowances = body
        .counts
        .into_iter()
        .map(|c| {
            let share =
                ctx.reconcile
                    .report(&body.replica_id, c.principal_id, c.requests, c.tokens, now);
            AllowanceWire {
                principal_id: c.principal_id,
                requests_share: share.requests,
                tokens_share: share.tokens,
            }
        })
        .collect();
    Ok(Json(ReconcileResponse { allowances }))
}

/// Rebuild immediately after a write so revocation is bounded by the proxy's
/// poll interval alone, not by poll interval plus rebuild interval.
///
/// The database write this follows has already committed by the time this
/// runs — every caller is a route that just did its `INSERT`/`UPDATE`/
/// `DELETE` and returned success to the client — so a rebuild failure here
/// must not turn that already-successful write into a 5xx: the operator
/// asked for a real change and got one, and reporting failure would be a
/// lie in the other direction. What must not happen is *silence*: a failed
/// rebuild means the published snapshot (and, in `--role all`, the routing
/// `Registry` built from it — see `SnapshotSink`) has just fallen out of
/// sync with the database, and nothing else notices until either this route
/// is hit again or `spawn_snapshot_rebuilder`'s next tick happens to
/// succeed. So: log it loudly, at error level, with the cause, and bump a
/// counter `GET /admin/health` reports — cheap enough to always run, and it
/// turns "silently wrong until someone notices by accident" into something
/// an operator (or an alert on that endpoint) can actually see.
async fn refresh(ctx: &Ctx) {
    match build_snapshot(&ctx.pool, &ctx.key).await {
        Ok(snap) => {
            ctx.cache.store_snapshot(snap);
        }
        Err(e) => {
            ctx.snapshot_rebuild_failures
                .fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                error = %e,
                "snapshot rebuild after an admin API write failed: the write itself already \
                 committed, but the published snapshot is now stale until the next successful \
                 rebuild; see GET /admin/health for how many rebuilds have failed"
            );
        }
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

/// Shared by every route gated on the proxy's own bootstrap token —
/// `/snapshot` and `/usage` alike. One implementation so a second, subtly
/// different bearer-check (a `==` that forgot why `constant_time_eq` exists,
/// say) can never be written for the route added after this one.
///
/// `/snapshot` discloses every key hash and usable upstream backend
/// credentials (see the schema comment on `model_backends.upstream_api_key`);
/// `/usage` accepts writes from anything holding this token. Both are worth
/// paying for a non-short-circuiting compare rather than plain `==`.
///
/// An empty `expected` or an empty presented token is rejected explicitly,
/// before ever reaching `constant_time_eq` — `constant_time_eq(b"", b"")` is
/// `true`, so without this an unset `--proxy-token` (`unwrap_or_default`
/// turns it into `""`) would authenticate *any* caller that sent
/// `Authorization: Bearer ` with nothing after it, including no bearer
/// value at all once stripped. An absent or empty token must mean "no one
/// can authenticate", never "everyone can" — `main.rs` additionally refuses
/// to start `--role control`/`all` at all without a non-empty token (see
/// `require_proxy_token`), so `expected` being empty here should be
/// unreachable in production; this check is what makes that a property this
/// function itself guarantees, rather than one only true because every
/// caller happens to uphold it.
fn proxy_token_authorised(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if !token.is_empty() => constant_time_eq(token.as_bytes(), expected.as_bytes()),
        _ => false,
    }
}

async fn get_snapshot(State(ctx): State<Ctx>, headers: HeaderMap) -> impl IntoResponse {
    if !proxy_token_authorised(&headers, &ctx.proxy_token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let snap = ctx.cache.current_snapshot();
    let etag = format!("\"{}\"", snap.version);
    if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    ([("etag", etag)], Json(snap.as_ref().to_wire())).into_response()
}

#[derive(Deserialize)]
struct UsageBatchRequest {
    events: Vec<UsageEvent>,
}

#[derive(Debug, Serialize)]
struct UsageBatchResponse {
    accepted: usize,
    /// Rows whose `principal_id` or `model` did not resolve to a live row —
    /// see `post_usage`'s doc comment for why this is a silent per-row drop
    /// rather than a failed batch.
    dropped: usize,
}

/// `POST /usage`: the Snapshot protocol's reverse channel (design doc,
/// "Snapshot protocol"). Batched, fire-and-forget, gated by the same proxy
/// bootstrap token as `/snapshot` — a stolen proxy token can already read
/// every key hash and usable backend credential, so letting it also write
/// usage rows grants nothing new.
///
/// Defined in P0 even though nothing sends to it until P2
/// (`usage::UsageReporter`, currently unwired) sends anything, so the wire
/// shape does not have to change once something does.
///
/// **A malformed batch must not 500 the control plane.** Two distinct kinds
/// of "malformed" are handled differently on purpose:
/// - A body that is not valid JSON, or is missing a required field, fails
///   `Json` extraction before this function runs and axum answers 400 — the
///   caller's mistake, reported as such, no partial processing possible.
/// - A structurally valid record whose `principal_id` or `model` name does
///   not match a live row (a key revoked and its principal deleted, or a
///   model renamed, between the request being made and the batch flushing)
///   is dropped from the batch rather than failing the whole thing. The
///   alternative — one bad id from a stale replica poisoning a whole
///   flush interval's worth of otherwise-good usage rows from every other
///   principal — is worse than losing the one row, and losing the one row
///   is already the design's stated tradeoff for usage in general ("dropping
///   usage rather than blocking a request is deliberate").
///
/// The `JOIN` below is what implements the per-row drop: a `principal_id` or
/// `model` name that does not match is simply absent from the joined result,
/// so it never reaches the `INSERT`, and every other row in the same batch is
/// unaffected.
async fn post_usage(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Json(body): Json<UsageBatchRequest>,
) -> Result<Json<UsageBatchResponse>, ApiError> {
    if !proxy_token_authorised(&headers, &ctx.proxy_token) {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "bad or missing proxy token",
        ));
    }

    let submitted = body.events.len();
    if body.events.is_empty() {
        return Ok(Json(UsageBatchResponse {
            accepted: 0,
            dropped: 0,
        }));
    }

    let mut principal_ids = Vec::with_capacity(submitted);
    let mut models = Vec::with_capacity(submitted);
    let mut prompt_tokens = Vec::with_capacity(submitted);
    let mut completion_tokens = Vec::with_capacity(submitted);
    let mut at = Vec::with_capacity(submitted);
    for e in &body.events {
        principal_ids.push(e.principal_id as i64);
        models.push(e.model.clone());
        prompt_tokens.push(e.prompt_tokens as i64);
        completion_tokens.push(e.completion_tokens as i64);
        at.push(e.at);
    }

    // `UNNEST` turns the five parallel arrays back into rows, positionally —
    // this is what lets one round trip insert a whole batch instead of one
    // query per event. The `JOIN`s against `principals`/`models` (rather than
    // a `NOT NULL` foreign key the `INSERT` could violate) are what makes an
    // unresolvable row silently absent from `RETURNING` instead of failing
    // the statement outright.
    let accepted_rows: Vec<(i64, i64, i64, i64)> = sqlx::query_as(
        "WITH input AS (
            SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::bigint[], $4::bigint[], $5::timestamptz[])
                AS t(principal_id, model_name, prompt_tokens, completion_tokens, at)
         )
         INSERT INTO usage_events (principal_id, model_id, prompt_tokens, completion_tokens, at)
         SELECT i.principal_id, m.id, i.prompt_tokens, i.completion_tokens, i.at
         FROM input i
         JOIN principals p ON p.id = i.principal_id
         JOIN models m ON m.name = i.model_name
         RETURNING id, principal_id, prompt_tokens, completion_tokens",
    )
    .bind(&principal_ids)
    .bind(&models)
    .bind(&prompt_tokens)
    .bind(&completion_tokens)
    .bind(&at)
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("usage ingestion", &e))?;

    let accepted = accepted_rows.len();
    apply_usage_to_budgets(&ctx.pool, &accepted_rows).await;

    Ok(Json(UsageBatchResponse {
        accepted,
        dropped: submitted - accepted,
    }))
}

/// The other half of `budgets.tokens_used` being "the running counter the
/// snapshot carries, reconciled from [`usage_events`]" (migrations/0005's
/// comment): every accepted event's tokens are folded into its principal's
/// budget row, if one exists. `principal_id` here is not `pub(crate)` typed
/// as `PrincipalId` — this is raw `i64` straight off the `RETURNING` clause,
/// matching every other id in this file.
///
/// One `UPDATE` per *distinct* principal in the batch, not per event: a
/// batch is typically many events for a handful of principals (P2's
/// reconciliation-scale traffic, not one row per caller), so grouping first
/// keeps this proportional to the number of principals reporting, not the
/// number of requests they made.
///
/// A failure here is logged and otherwise swallowed rather than turning an
/// already-successful usage ingest into a 500: the durable audit log
/// (`usage_events`) is already committed by the time this runs, and a
/// missed budget increment only means enforcement is stale until the next
/// successful one — not that billing data was lost. This mirrors `refresh`'s
/// same reasoning a few lines below.
async fn apply_usage_to_budgets(pool: &PgPool, accepted_rows: &[(i64, i64, i64, i64)]) {
    let mut totals: HashMap<i64, i64> = HashMap::new();
    for (_id, principal_id, prompt_tokens, completion_tokens) in accepted_rows {
        *totals.entry(*principal_id).or_insert(0) += prompt_tokens + completion_tokens;
    }
    for (principal_id, total) in totals {
        if total <= 0 {
            continue;
        }
        if let Err(e) = sqlx::query(
            "UPDATE budgets SET tokens_used = tokens_used + $2, updated_at = now()
             WHERE principal_id = $1",
        )
        .bind(principal_id)
        .bind(total)
        .execute(pool)
        .await
        {
            tracing::error!(
                error = %e,
                principal_id,
                total,
                "could not apply reported usage to that principal's budget; usage_events is \
                 unaffected, but budget enforcement is now stale for this principal until the \
                 next successful update"
            );
        }
    }
}

#[derive(Serialize)]
struct HealthView {
    /// See `Ctx::snapshot_rebuild_failures` and `refresh`'s doc comment: a
    /// nonzero value means the database and the published snapshot have, at
    /// some point, fallen out of sync. It does not by itself mean they still
    /// are — the next successful rebuild (another write, or
    /// `spawn_snapshot_rebuilder`'s next tick) resolves it — but the counter
    /// itself never resets, so it stays a true "has this ever happened"
    /// signal an operator or an alert can watch for.
    snapshot_rebuild_failures: u64,
}

// --- Admin session authentication (P4) -------------------------------

/// Read the session token out of the `Cookie` header, if present. Manual
/// parsing rather than a cookie-jar crate: the request side only ever needs
/// to find one named value in a `key=value; key=value` list, which is a
/// handful of lines and one fewer dependency.
fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == crate::control::auth::SESSION_COOKIE).then(|| value.to_string())
    })
}

/// `Set-Cookie` for a freshly created session. `HttpOnly` so client-side JS
/// (including an XSS payload in the admin UI itself) cannot read the token;
/// `SameSite=Strict` because this cookie only ever needs to be sent on
/// same-site navigations/requests the admin UI itself makes; `Secure` only
/// when TLS is actually on, per `Ctx::tls_enabled`'s doc comment.
fn set_cookie_header(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}",
        crate::control::auth::SESSION_COOKIE,
        crate::control::auth::SESSION_TTL_HOURS * 3600,
    )
}

/// `Set-Cookie` for logout: same name/attributes, empty value, immediately
/// expired — the standard way to make a browser drop a cookie it already
/// holds.
fn clear_cookie_header(secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{secure}",
        crate::control::auth::SESSION_COOKIE,
    )
}

#[derive(Deserialize)]
struct LoginRequest {
    name: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    name: String,
}

/// `POST /login`: the one `/admin`-adjacent route reachable without a
/// session — logging in is how a session is obtained in the first place.
/// Deliberately outside the `/admin/*` prefix `require_session` gates, and
/// mounted alongside it rather than requiring the proxy token: this is a
/// human typing a password into the UI, not a proxy or an operator's script
/// carrying `--proxy-token`.
async fn login(
    State(ctx): State<Ctx>,
    Json(body): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(principal_id) =
        crate::control::auth::verify_login(&ctx.pool, &body.name, &body.password).await
    else {
        // Same message regardless of *why* — unknown name, wrong password,
        // disabled principal, or a service_account that has no password at
        // all. Distinguishing any of those to the caller is exactly the
        // username-enumeration mistake a login endpoint must not make.
        return Err(api_error(StatusCode::UNAUTHORIZED, "invalid credentials"));
    };
    let token = crate::control::auth::create_session(&ctx.pool, principal_id)
        .await
        .map_err(|e| db_error("create session", &e))?;
    let headers = [(
        axum::http::header::SET_COOKIE,
        set_cookie_header(&token, ctx.tls_enabled),
    )];
    Ok((headers, Json(LoginResponse { name: body.name })))
}

/// `POST /logout`. Always answers 204, whether or not the presented cookie
/// (if any) named a real session — logging out is idempotent from the
/// caller's point of view, and the browser is told to drop the cookie
/// either way.
async fn logout(State(ctx): State<Ctx>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = session_cookie(&headers) {
        let _ = crate::control::auth::delete_session(&ctx.pool, &token).await;
    }
    (
        [(
            axum::http::header::SET_COOKIE,
            clear_cookie_header(ctx.tls_enabled),
        )],
        StatusCode::NO_CONTENT,
    )
}

/// Gates every `/admin/*` route (mounted as a `Router` layer, see `serve`
/// below): no valid session cookie, no response body — a 401 before the
/// wrapped handler ever runs. This is the fix for the gap `TODO.md` has
/// documented since P0: "`/admin/*` and `/snapshot` are gated on the shared
/// proxy token alone" (`/snapshot`, `/usage` and `/limits/reconcile` still
/// are, deliberately — those are proxy processes, not humans, and have no
/// password to present).
///
/// Authenticates *who* is calling and stops there — it deliberately does not
/// decide *what* they may do. That is `RequirePermission`'s job, run as an
/// extractor inside each handler once the session is known to be real. The
/// two are split into separate layers rather than one, because "no session"
/// and "a session, but not permitted to do this" are different failures a
/// caller needs to tell apart (401 vs 403), and because a route can only
/// know which permission it needs from inside its own handler — this
/// `from_fn` layer has no way to see which of the many routes it wraps a
/// given request is bound for.
async fn require_session(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, ApiError> {
    let Some(token) = session_cookie(&headers) else {
        return Err(api_error(StatusCode::UNAUTHORIZED, "no session cookie"));
    };
    let Some(principal_id) = crate::control::auth::authenticate_session(&ctx.pool, &token).await
    else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "invalid or expired session",
        ));
    };
    // Stashed in the request's extensions rather than threaded through as a
    // handler argument from here: this middleware runs once per request
    // before axum has even matched which handler it is bound for, so this is
    // the only place a resolved principal id has to go. `RequirePermission`
    // (below) reads it back out inside the matched handler's own extractor
    // chain.
    request
        .extensions_mut()
        .insert(AdminPrincipal(principal_id));
    Ok(next.run(request).await)
}

/// The authenticated caller of an `/admin/*` request — `require_session`'s
/// output and `RequirePermission`'s input. A newtype rather than a bare
/// `i64` so it cannot be confused with some other `i64` extension a future
/// route adds, and so it is not `Clone`-and-forgettable the way threading a
/// raw id through would be.
#[derive(Debug, Clone, Copy)]
struct AdminPrincipal(i64);

/// Every permission `/admin/*` routes check against, named for the
/// `permissions.verb` column value each one maps to. Deliberately only
/// these four (plus `model:invoke`, irrelevant here — that one gates the
/// *data* plane, resolved once per snapshot build in
/// `control::build::flatten_grants`, not checked per admin request): the
/// vocabulary `migrations/0001_init.sql` already seeds, not a parallel
/// permission scheme invented for this file. A route that does not fit any
/// of the three specific verbs (`key:create`, `key:revoke`) falls back to
/// `config:write` — the schema has no finer-grained permission for
/// "manage principals" or "manage virtual models" than that, and inventing
/// one per table would multiply roles for no operator-visible benefit; see
/// `admin_routes` in `serve` for the full mapping, route by route.
mod admin_permission {
    /// Listing/viewing state: `GET /admin/*`. Named for `usage:read`
    /// because that is the one read-shaped verb migration 0001 seeds — nothing
    /// admin-visible needs a second one when this covers keys, principals,
    /// models, roles, limits and budgets equally well; none of them expose
    /// anything more sensitive than usage itself already does.
    pub const READ: &str = "usage:read";
    pub const KEY_CREATE: &str = "key:create";
    pub const KEY_REVOKE: &str = "key:revoke";
    /// Every other admin write: principals, roles, models, backends, virtual
    /// models, routing rules and targets, limits, budgets, passwords.
    pub const CONFIG_WRITE: &str = "config:write";
}

/// `true` if `principal_id` holds a role granting `verb` on `'*'` — every
/// admin permission migration 0001 seeds is scoped to `'*'`, unlike
/// `model:invoke` (`model/*`), so there is nothing finer to match against
/// resource here the way `Principal::may_invoke` matches model names.
async fn principal_has_permission(
    pool: &PgPool,
    principal_id: i64,
    verb: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM principal_roles pr
             JOIN role_permissions rp ON rp.role_id = pr.role_id
             JOIN permissions p ON p.id = rp.permission_id
             WHERE pr.principal_id = $1 AND p.verb = $2
         )",
    )
    .bind(principal_id)
    .bind(verb)
    .fetch_one(pool)
    .await
}

/// One zero-sized marker type per permission `RequirePermission` (below) can
/// be instantiated with. A `const VERB: &'static str` generic parameter
/// would say the same thing more directly, but stable Rust at this crate's
/// pinned `rust-version = "1.82"` does not allow `&str` as a const generic
/// (`adt_const_params` is nightly-only) — a trait with an associated
/// constant is the stable equivalent, at the cost of one marker struct per
/// permission instead of one string literal.
trait AdminPermission {
    const VERB: &'static str;
}

struct ReadPermission;
impl AdminPermission for ReadPermission {
    const VERB: &'static str = admin_permission::READ;
}

struct KeyCreatePermission;
impl AdminPermission for KeyCreatePermission {
    const VERB: &'static str = admin_permission::KEY_CREATE;
}

struct KeyRevokePermission;
impl AdminPermission for KeyRevokePermission {
    const VERB: &'static str = admin_permission::KEY_REVOKE;
}

struct ConfigWritePermission;
impl AdminPermission for ConfigWritePermission {
    const VERB: &'static str = admin_permission::CONFIG_WRITE;
}

/// An extractor, not a second `from_fn` middleware layer: which permission a
/// request needs depends on which route matched, and axum's `from_fn`
/// layers run *before* routing has picked a handler, so they cannot see
/// that. Adding `RequireRead`/`RequireKeyCreate`/`RequireKeyRevoke`/
/// `RequireConfigWrite` (the aliases below) as a handler argument runs this
/// once axum already knows which handler — and therefore which permission —
/// applies, and its `Rejection = ApiError` turns a missing grant into 403
/// automatically, the same way a bad `Path<i64>` already turns into 400
/// without every handler writing that check by hand.
struct RequirePermission<P>(std::marker::PhantomData<P>);

// A hand-written `impl` rather than `#[derive(Default)]`: the derive would
// add a `where P: Default` bound that `P` (a zero-sized marker type that
// never needs to be constructed on its own) has no reason to satisfy.
// `RequireRead::default()` and friends are how the tests below build one of
// these directly, bypassing `FromRequestParts` — a generic type *alias*
// bound to a concrete `P` cannot be called as a tuple-struct constructor in
// Rust (`RequireRead::default()` does not compile, only
// `RequirePermission::<ReadPermission>(..)` or this would), so `default()`
// is the ergonomic way to spell "the permission this alias names, satisfied
// unconditionally" at each of those call sites.
impl<P> Default for RequirePermission<P> {
    fn default() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<P> FromRequestParts<Ctx> for RequirePermission<P>
where
    P: AdminPermission + Send + Sync + 'static,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Ctx) -> Result<Self, Self::Rejection> {
        // `require_session` (the `from_fn` layer every `/admin/*` route is
        // wrapped in) always runs first and always inserts this — its
        // absence here would mean this extractor is used on a route that
        // is not behind that layer, a wiring bug rather than a caller
        // error, so a 500 rather than a 401/403 correctly signals that
        // rather than being mistaken for an unauthenticated request.
        let Some(AdminPrincipal(principal_id)) = parts.extensions.get::<AdminPrincipal>().copied()
        else {
            tracing::error!(
                "RequirePermission ran with no AdminPrincipal in request extensions; \
                 is this route mounted outside require_session?"
            );
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authorisation check misconfigured; see server logs",
            ));
        };
        let allowed = principal_has_permission(&state.pool, principal_id, P::VERB)
            .await
            .map_err(|e| db_error("checking admin permission", &e))?;
        if allowed {
            Ok(Self(std::marker::PhantomData))
        } else {
            Err(api_error(
                StatusCode::FORBIDDEN,
                format!(
                    "principal {principal_id} lacks the {:?} permission",
                    P::VERB
                ),
            ))
        }
    }
}

/// Named aliases for `RequirePermission`'s four concrete instantiations —
/// every admin handler below takes one of these as a parameter rather than
/// spelling out `RequirePermission<SomeMarker>`, purely for readability at
/// each call site.
type RequireRead = RequirePermission<ReadPermission>;
type RequireKeyCreate = RequirePermission<KeyCreatePermission>;
type RequireKeyRevoke = RequirePermission<KeyRevokePermission>;
type RequireConfigWrite = RequirePermission<ConfigWritePermission>;

#[derive(Deserialize)]
struct SetPassword {
    password: String,
}

/// `PUT /admin/principals/{id}/password`: sets or replaces a principal's
/// login password, promoting it to `kind = 'user'` if it was not already —
/// see `auth::set_password`. Gated by `require_session` like every other
/// `/admin/*` route; bootstrapping the very *first* user (when no session
/// can possibly exist yet) is `fastllm-proxy set-password`'s job instead
/// (`main.rs`), not this route's.
///
/// Also gated by `RequireConfigWrite`, deliberately not something narrower:
/// this is the route that promotes a principal to `kind = 'user'` and hands
/// it a working login, which is exactly the step a service-account-turned-UI
/// -viewer's password must not silently pass through unauthorised (see the
/// design note on the real-production bug this closes — every principal
/// with a password was previously a full admin, because nothing here
/// checked anything at all). Requiring `config:write` means only a caller
/// already trusted to reconfigure the system can hand out a new login,
/// which is the same trust boundary the schema already draws for every
/// other administrative write.
async fn put_password(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
    Json(body): Json<SetPassword>,
) -> Result<StatusCode, ApiError> {
    if body.password.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "password must not be empty",
        ));
    }
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM principals WHERE id = $1)")
        .bind(id)
        .fetch_one(&ctx.pool)
        .await
        .map_err(|e| db_error("check principal", &e))?;
    if !exists {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no principal with id {id}"),
        ));
    }
    crate::control::auth::set_password(&ctx.pool, id, &body.password)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "admin API password hashing failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "setting password failed; see server logs",
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_health(State(ctx): State<Ctx>, _perm: RequireRead) -> Json<HealthView> {
    Json(HealthView {
        snapshot_rebuild_failures: ctx.snapshot_rebuild_failures.load(Ordering::Relaxed),
    })
}

/// `GET /healthz`: the Kubernetes probe target (`deploy/control.yaml`'s
/// readiness/liveness/startup probes), not a second copy of `/admin/health`.
///
/// Two things force it to be a distinct, separate route rather than just
/// pointing the probes at `/admin/health`:
///
/// - The kubelet has no credential. `/admin/health` sits behind
///   `require_session` like every other `/admin/*` route, and a probe cannot
///   present a session cookie — there is no login step in a liveness check.
///   `/healthz` is therefore mounted on `public_routes` in `serve`, *not*
///   nested under `admin_routes`, so it is exempt from `require_session` by
///   construction rather than by an exception carved into that middleware.
/// - `/admin/health`'s body (`HealthView`) is meant for an authenticated
///   operator and is free to grow richer over time. A probe response is the
///   opposite: it must stay small, boring, and disclose nothing an
///   unauthenticated caller on the network shouldn't see — no key material,
///   no credentials, no backend addresses, no principal names. `{"status":
///   "ok"}` is deliberately all this returns, unlike the data-plane's
///   `GET /health` (`control::api`'s proxy-side counterpart, which lists
///   backends) — this route must never grow to do that.
///
/// It does no I/O — no database query, nothing awaited — on purpose.
/// `readinessProbe`/`livenessProbe` fire every few seconds for the life of
/// the pod; routing that through Postgres would make a probe interval into a
/// steady background query load, and worse, a transient database blip would
/// fail liveness and get a control plane that is otherwise fine restarted
/// for a problem restarting it does nothing to fix. Touching
/// `ctx.snapshot_rebuild_failures` (an `Arc<AtomicU64>`, loaded with
/// `Ordering::Relaxed`) costs nothing and proves the handler actually ran
/// against real shared state rather than being dead code — but it is *not*
/// returned in the body, matching the "discloses nothing" rule above; a
/// wedged process (deadlocked, task starved) simply never gets here to
/// return a response at all, which is what a liveness probe needs to catch.
async fn healthz(State(ctx): State<Ctx>) -> Json<serde_json::Value> {
    let _ = ctx.snapshot_rebuild_failures.load(Ordering::Relaxed);
    Json(serde_json::json!({ "status": "ok" }))
}

pub async fn serve(
    pool: PgPool,
    addr: SocketAddr,
    proxy_token: String,
    cache: Arc<dyn SnapshotSink>,
    key: Arc<EncryptionKey>,
    tls: Option<rustls::ServerConfig>,
) -> anyhow::Result<()> {
    let tls_enabled = tls.is_some();
    let ctx = Ctx {
        pool,
        proxy_token,
        cache,
        key,
        snapshot_rebuild_failures: Arc::new(AtomicU64::new(0)),
        reconcile: Arc::new(ReconcileState::new()),
        tls_enabled,
    };
    // Every mutating route below ends in `refresh(&ctx)` — the one write
    // path that publishes through `SnapshotSink::store_snapshot`. That is
    // what keeps the published snapshot and (in `--role all`) the routing
    // `Registry` from ever disagreeing about what was just changed; adding a
    // route that writes a row without it would reintroduce exactly the
    // "changed in Postgres, invisible to a running process" gap that
    // `spawn_snapshot_rebuilder` exists to paper over.
    //
    // `admin_routes` is everything `require_session` gates — every
    // `/admin/*` route including the UI's own data source — layered with
    // that middleware exactly once here rather than checked inside each
    // handler, so a new route added later is protected by construction
    // instead of by remembering to add a check. `require_session` only
    // establishes *who* is calling, though (see its own doc comment); *what*
    // they may do is each handler's own `RequireRead`/`RequireKeyCreate`/
    // `RequireKeyRevoke`/`RequireConfigWrite` argument, so — unlike session
    // gating — a route added here without also adding one of those
    // extractors to its handler is reachable by any authenticated principal
    // regardless of role. There is no equivalent "layered once" trick for
    // that half, because the permission a route needs depends on which one
    // it is (`GET` vs `POST /admin/keys` at the very first line below need
    // different permissions despite sharing a path), which per-route
    // middleware can't see and per-handler extractors can.
    let admin_routes = Router::new()
        .route("/admin/keys", get(list_keys).post(post_key))
        .route("/admin/keys/{id}", delete(revoke_key))
        .route(
            "/admin/principals",
            get(list_principals).post(post_principal),
        )
        .route("/admin/principals/{id}", delete(delete_principal))
        .route("/admin/principals/{id}/roles", post(grant_role))
        .route("/admin/principals/{id}/roles/{role}", delete(revoke_role))
        .route("/admin/principals/{id}/password", put(put_password))
        .route("/admin/models", get(list_models).post(post_model))
        .route("/admin/models/{id}", delete(delete_model))
        .route("/admin/models/{id}/backends", post(post_backend))
        .route("/admin/backends/{id}", delete(delete_backend))
        .route(
            "/admin/virtual-models",
            get(list_virtual_models).post(post_virtual_model),
        )
        .route("/admin/virtual-models/{id}", delete(delete_virtual_model))
        .route(
            "/admin/prompt-classes",
            get(list_prompt_classes).post(post_prompt_class),
        )
        .route("/admin/prompt-classes/{id}", delete(delete_prompt_class))
        .route(
            "/admin/prompt-classes/{id}/examples",
            post(post_prompt_class_example),
        )
        .route(
            "/admin/prompt-classes/evaluate",
            post(evaluate_prompt_classes),
        )
        .route(
            "/admin/fallback-model",
            get(get_fallback_model).put(put_fallback_model),
        )
        .route("/admin/virtual-models/{id}/rules", post(post_rule))
        .route("/admin/rules/{id}", delete(delete_rule))
        .route("/admin/rules/{id}/targets", post(post_rule_target))
        .route("/admin/rule-targets/{id}", delete(delete_rule_target))
        .route(
            "/admin/virtual-models/{id}/defaults",
            post(post_default_target),
        )
        .route(
            "/admin/virtual-model-defaults/{id}",
            delete(delete_default_target),
        )
        .route("/admin/roles", get(list_roles))
        .route("/admin/limits", get(list_limits))
        .route(
            "/admin/principals/{id}/limits",
            put(put_limits).delete(delete_limits),
        )
        .route("/admin/budgets", get(list_budgets))
        .route(
            "/admin/principals/{id}/budget",
            put(put_budget).delete(delete_budget),
        )
        .route("/admin/health", get(admin_health))
        .layer(axum::middleware::from_fn_with_state(
            ctx.clone(),
            require_session,
        ));

    // Reachable with no session: `/login` (how one is obtained in the first
    // place), the three proxy-token-gated routes proxy processes (not
    // humans) call — see their own doc comments for why those stay on the
    // bearer token rather than moving to sessions — and `/healthz`, which has
    // neither a session cookie nor the proxy token available to it (see that
    // handler's doc comment for why it is a separate route from
    // `/admin/health` rather than reusing it).
    let public_routes = Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/snapshot", get(get_snapshot))
        .route("/usage", post(post_usage))
        .route("/limits/reconcile", post(post_reconcile))
        .route("/healthz", get(healthz));

    // The management UI (P4): served by this process only, which is to say
    // only by `--role control`/`all` — `serve` is never called for `--role
    // proxy` at all, so there is no separate role check to get wrong here.
    // Mounted last as the fallback: any path not matched above (`/`,
    // `/ui/...`, a client-side route the SPA itself resolves) falls through
    // to `crate::control::ui`, which degrades to a plain "UI not available"
    // response if `web/dist/` was empty at build time (see that module).
    let app = admin_routes
        .merge(public_routes)
        .with_state(ctx)
        .fallback(crate::control::ui::serve_asset);

    match tls {
        Some(server_config) => {
            let tcp = tokio::net::TcpListener::bind(addr).await?;
            let listener = crate::control::tls::TlsListener::new(tcp, server_config);
            axum::serve(listener, app).await?;
        }
        None => {
            // `/snapshot` carries usable upstream credentials (see the schema
            // comment on `model_backends.upstream_api_key`) and `/usage`
            // accepts writes gated by the same bearer token — plain HTTP
            // means both travel, and the token that gates them, in the
            // clear. Not fatal: a dev deployment with no real backend
            // credentials is legitimate, so this warns loudly instead of
            // refusing to start.
            tracing::warn!(
                "no --tls-cert/--tls-key configured; /snapshot and /usage are being served over \
                 plain HTTP. /snapshot carries usable upstream backend credentials — this must \
                 not be used wherever a backend has a real credential."
            );
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared with `control::import::tests` — see `secrets::test_key` for
    /// why every DB-backed test in `control::*` must use the same key
    /// rather than each picking its own.
    use crate::control::secrets::test_key;
    use crate::control::test_support::TestCleanup;

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
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "task6-test-principal-");
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
            snapshot_rebuild_failures: Arc::new(AtomicU64::new(0)),
            reconcile: Arc::new(ReconcileState::new()),
            tls_enabled: false,
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
        // Redundant with the explicit `delete_*` calls near the end of the
        // happy path below, and that's fine — a second `DELETE` matching
        // nothing is a no-op. What this buys is the case those calls never
        // reach: any `assert!`/`unwrap()` above them panicking first.
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "route-principal-")
            .track_prefix("models", "name", "route-model-");
        let principal_name = unique_name("route-principal");
        let model_name = unique_name("route-model");

        let (status, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite::default(),
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
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(RoleGrant {
                role: "inference".into(),
            }),
        )
        .await
        .unwrap();

        let (_, model) = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
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
            RequireConfigWrite::default(),
            Path(model_id),
            Json(NewBackend::openai(
                "http://route-test:8000/v1/",
                Some(upstream_credential.into()),
            )),
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
        let listed = list_models(State(ctx.clone()), RequireRead::default())
            .await
            .unwrap();
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
            RequireConfigWrite::default(),
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

        delete_backend(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(backend_id),
        )
        .await
        .unwrap();
        delete_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(model_id),
        )
        .await
        .unwrap();
        delete_principal(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
        )
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
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "key-listing-principal-");
        let principal_name = unique_name("key-listing-principal");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite::default(),
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

        let listed = list_keys(State(ctx.clone()), RequireRead::default())
            .await
            .unwrap();
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
            delete_model(State(ctx.clone()), RequireConfigWrite::default(), Path(-1))
                .await
                .unwrap_err(),
            delete_backend(State(ctx.clone()), RequireConfigWrite::default(), Path(-1))
                .await
                .unwrap_err(),
            delete_principal(State(ctx.clone()), RequireConfigWrite::default(), Path(-1))
                .await
                .unwrap_err(),
            revoke_key(State(ctx.clone()), RequireKeyRevoke::default(), Path(-1))
                .await
                .unwrap_err(),
        ] {
            assert_eq!(status, StatusCode::NOT_FOUND);
            let message = body.0["error"].as_str().unwrap();
            assert!(message.contains("-1"), "must name the id: {message}");
        }

        let (status, body) = grant_role(
            State(ctx.clone()),
            RequireConfigWrite::default(),
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
            RequireConfigWrite::default(),
            Path(-1),
            Json(NewBackend::openai("http://x:8000/v1", None)),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("-1"));

        let (status, _) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite::default(),
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
        let roles = list_roles(State(ctx), RequireRead::default())
            .await
            .unwrap();
        let inference = roles.0.iter().find(|r| r.name == "inference").unwrap();
        assert!(inference
            .permissions
            .iter()
            .any(|p| p.verb == "model:invoke" && p.resource == "model/*"));
        let admin = roles.0.iter().find(|r| r.name == "admin").unwrap();
        assert!(admin.permissions.len() > inference.permissions.len());
    }

    /// Every virtual-model route, end to end: creating a virtual model, a
    /// rule on it, a target on that rule, and a default target all publish
    /// through the same `refresh()` -> `SnapshotSink::store_snapshot` path
    /// every other admin route uses — same invariant
    /// `every_mutating_route_publishes_through_the_one_write_path` pins for
    /// keys/models/principals, applied to the P1 tables.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn virtual_model_routes_publish_rules_and_targets_to_the_snapshot() {
        let (ctx, cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("models", "name", "vm-route-primary-")
            .track_prefix("models", "name", "vm-route-secondary-")
            .track_prefix("virtual_models", "name", "vm-route-canary-");
        let primary_name = unique_name("vm-route-primary");
        let secondary_name = unique_name("vm-route-secondary");

        let (_, primary) = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: primary_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        let primary_id = primary.0["id"].as_i64().unwrap();

        let (_, secondary) = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: secondary_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        let secondary_id = secondary.0["id"].as_i64().unwrap();

        let vm_name = unique_name("vm-route-canary");
        let (status, vm) = post_virtual_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewVirtualModel {
                name: vm_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        let vm_id = vm.0["id"].as_i64().unwrap();

        let (_, rule) = post_rule(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(vm_id),
            Json(NewRule {
                position: 0,
                match_condition: MatchConditionJson {
                    roles: vec!["canary".into()],
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap();
        let rule_id = rule.0["id"].as_i64().unwrap();

        let _ = post_rule_target(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(rule_id),
            Json(NewTarget {
                model_id: primary_id,
                weight: 100,
                position: 0,
            }),
        )
        .await
        .unwrap();

        let _ = post_default_target(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(vm_id),
            Json(NewTarget {
                model_id: secondary_id,
                weight: 100,
                position: 0,
            }),
        )
        .await
        .unwrap();

        let snap = cache.current_snapshot();
        let published = snap
            .virtual_models
            .get(&vm_name)
            .expect("the virtual model created over the admin API must be in the snapshot");
        assert_eq!(published.rules.len(), 1);
        assert_eq!(
            published.rules[0].conditions.caller.roles,
            ["canary".to_string()].into_iter().collect()
        );
        assert_eq!(published.rules[0].targets[0].model, primary_name);
        assert_eq!(published.default_targets[0].model, secondary_name);

        // `GET /admin/virtual-models` mirrors the same rows.
        let listed = list_virtual_models(State(ctx.clone()), RequireRead::default())
            .await
            .unwrap();
        let listed_vm = listed.0.iter().find(|v| v.name == vm_name).unwrap();
        assert_eq!(listed_vm.rules.len(), 1);
        assert_eq!(listed_vm.rules[0].targets[0].model, primary_name);
        assert_eq!(listed_vm.default_targets[0].model, secondary_name);

        // Deleting the virtual model cascades its rules and targets, and the
        // deletion reaches the published snapshot the same way creation did.
        delete_virtual_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(vm_id),
        )
        .await
        .unwrap();
        assert!(
            !cache
                .current_snapshot()
                .virtual_models
                .contains_key(&vm_name),
            "a deleted virtual model must leave the published snapshot"
        );
    }

    /// The privilege-escalation trap this design explicitly calls out: a
    /// model and a virtual model must never be allowed to share a name,
    /// because the ambiguity would make `authorize`/`resolve_target_model`'s
    /// decision (see that function's doc comment) apply to the wrong one —
    /// whichever table happens to win a lookup, silently.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_model_and_a_virtual_model_cannot_share_a_name() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("models", "name", "vm-collision-")
            .track_prefix("virtual_models", "name", "vm-collision-");
        let name = unique_name("vm-collision");

        let (status, _) = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);

        let err = post_virtual_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewVirtualModel {
                name: name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        // And the reverse direction.
        let other_name = unique_name("vm-collision-reverse");
        let (status, _) = post_virtual_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewVirtualModel {
                name: other_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);

        let err = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: other_name,
                description: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
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
        let _cleanup = TestCleanup::new().track_prefix("models", "name", "rebuild-test-");

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

    /// Regression for the review finding: `refresh` used to be `if let
    /// Ok(snap) = build_snapshot(...)`, discarding the error entirely on
    /// failure — no log, no counter, and the mutating route it followed
    /// still answered success. A rebuild failure has to become observable
    /// somewhere, since the route itself correctly keeps reporting the
    /// write succeeded (the write did commit).
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_failed_refresh_is_counted_on_admin_health() {
        let (ctx, _cache) = test_ctx().await;
        let before = ctx
            .snapshot_rebuild_failures
            .load(std::sync::atomic::Ordering::Relaxed);

        // Force `build_snapshot`'s first query to fail deterministically,
        // without touching global state another concurrent test could race
        // on: closing this test's own pool handle.
        ctx.pool.close().await;

        refresh(&ctx).await;

        let after = ctx
            .snapshot_rebuild_failures
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after,
            before + 1,
            "a failed rebuild must increment the counter GET /admin/health reports"
        );

        let health = admin_health(State(ctx.clone()), RequireRead::default()).await;
        assert_eq!(health.0.snapshot_rebuild_failures, after);
    }

    fn auth_header(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    /// Regression for the review finding: `constant_time_eq(b"", b"")` is
    /// `true`, so `proxy_token.unwrap_or_default()` plus the old
    /// unconditional compare meant an unset `--proxy-token` authenticated
    /// *any* request carrying `Authorization: Bearer ` with nothing after
    /// it — an unset token must never be equivalent to "everyone is
    /// authorised". All three ways that used to slip through are checked
    /// here directly against `proxy_token_authorised`, independent of any
    /// particular route.
    #[test]
    fn an_empty_configured_or_presented_token_never_authorises() {
        // An empty configured token: no presented value can ever match it,
        // not even another empty one.
        assert!(!proxy_token_authorised(&auth_header(""), ""));
        assert!(!proxy_token_authorised(&HeaderMap::new(), ""));
        assert!(!proxy_token_authorised(&auth_header("anything"), ""));

        // A real configured token, but an empty presented one — exactly
        // `Authorization: Bearer ` with nothing after it.
        assert!(!proxy_token_authorised(&auth_header(""), "real-token"));

        // The header missing `Bearer ` entirely, or missing outright, must
        // also fail against a real configured token.
        assert!(!proxy_token_authorised(&HeaderMap::new(), "real-token"));

        // Sanity check the positive case still works, so the assertions
        // above are proving something rather than passing vacuously.
        assert!(proxy_token_authorised(
            &auth_header("real-token"),
            "real-token"
        ));
    }

    fn usage_event(principal_id: i64, model: &str) -> UsageEvent {
        UsageEvent {
            principal_id: principal_id as u64,
            model: model.to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            at: chrono::Utc::now(),
        }
    }

    /// `POST /usage` is the Snapshot protocol's reverse channel: a valid
    /// batch persists to `usage_events`, and — same as `/snapshot` — the
    /// route is gated on the proxy's bootstrap token, absent or wrong, with
    /// nothing persisted in either rejected case.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_valid_usage_batch_persists_and_the_route_is_gated_by_the_proxy_token() {
        let (ctx, _cache) = test_ctx().await;
        // Tags distinct enough that this cleanup's prefix match cannot also
        // catch `a_batch_with_an_unknown_principal_survives_and_only_that_row_is_dropped`'s
        // rows below — "usage-principal" would otherwise be a string prefix
        // of that test's "usage-principal-survives", and the two tests can
        // run concurrently under `cargo test`.
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "usage-basic-principal-")
            .track_prefix("models", "name", "usage-basic-model-");
        let principal_name = unique_name("usage-basic-principal");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewPrincipal {
                name: principal_name.clone(),
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        let principal_id = created.0["id"].as_i64().unwrap();

        let model_name = unique_name("usage-basic-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: model_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();

        // Wrong token: rejected, nothing persisted.
        let err = post_usage(
            State(ctx.clone()),
            auth_header("not-the-token"),
            Json(UsageBatchRequest {
                events: vec![usage_event(principal_id, &model_name)],
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);

        // Absent token: same result.
        let err = post_usage(
            State(ctx.clone()),
            HeaderMap::new(),
            Json(UsageBatchRequest {
                events: vec![usage_event(principal_id, &model_name)],
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);

        let count_before: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_events WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_one(&ctx.pool)
                .await
                .unwrap();
        assert_eq!(
            count_before, 0,
            "a rejected batch must not have written anything"
        );

        // The right token: accepted and persisted.
        let resp = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![usage_event(principal_id, &model_name)],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.accepted, 1);
        assert_eq!(resp.0.dropped, 0);

        let count_after: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_events WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_one(&ctx.pool)
                .await
                .unwrap();
        assert_eq!(count_after, 1);
    }

    /// The design's stated tradeoff, exercised directly: a batch containing a
    /// row for a principal that does not (or no longer) exist must not 500
    /// the control plane or fail the rows around it — the bad row is dropped
    /// and every other row in the same batch is still persisted.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_batch_with_an_unknown_principal_survives_and_only_that_row_is_dropped() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "usage-principal-survives-")
            .track_prefix("models", "name", "usage-model-survives-");
        let principal_name = unique_name("usage-principal-survives");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewPrincipal {
                name: principal_name.clone(),
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        let principal_id = created.0["id"].as_i64().unwrap();

        let model_name = unique_name("usage-model-survives");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: model_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();

        // Comfortably outside any id a bootstrap-seeded test database will
        // ever assign.
        let unknown_principal_id = -424_242;

        let resp = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![
                    usage_event(principal_id, &model_name),
                    usage_event(unknown_principal_id, &model_name),
                ],
            }),
        )
        .await
        .unwrap();

        assert_eq!(resp.0.accepted, 1, "the row for a real principal must land");
        assert_eq!(
            resp.0.dropped, 1,
            "the row naming a nonexistent principal must be dropped, not fail the batch"
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_events WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_one(&ctx.pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    /// An empty batch is a no-op, not an error — a proxy with nothing to
    /// report should not have to special-case that before calling this.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn an_empty_usage_batch_is_accepted_and_does_nothing() {
        let (ctx, _cache) = test_ctx().await;
        let resp = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest { events: vec![] }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.accepted, 0);
        assert_eq!(resp.0.dropped, 0);
    }

    // --- Rate limits (P2) --------------------------------------------

    async fn make_principal(ctx: &Ctx, name: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
        )
        .bind(name)
        .fetch_one(&ctx.pool)
        .await
        .unwrap()
    }

    /// The write path this whole file's other tests already pin for every
    /// other route: `put_limits` reaches the published snapshot without a
    /// second rebuild trigger, `delete_limits` removes it again, and the
    /// principal ends up unlimited afterward — not limited to zero.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn put_and_delete_limits_reach_the_published_snapshot() {
        let (ctx, cache) = test_ctx().await;
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "limits-route-principal-");
        let principal_id = make_principal(&ctx, &unique_name("limits-route-principal")).await;

        put_limits(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(PutLimits {
                requests_per_min: Some(7),
                tokens_per_min: None,
            }),
        )
        .await
        .unwrap();

        let published = cache
            .current_snapshot()
            .principals
            .get(&(principal_id as u64))
            .expect("the principal must be in the published snapshot")
            .limits;
        assert_eq!(
            published,
            Some(crate::limiter::Limits {
                requests_per_min: Some(7),
                tokens_per_min: None,
            })
        );

        let listed = list_limits(State(ctx.clone()), RequireRead::default())
            .await
            .unwrap();
        assert!(listed
            .0
            .iter()
            .any(|l| l.principal_id == principal_id && l.requests_per_min == Some(7)));

        delete_limits(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
        )
        .await
        .unwrap();
        let published_after_delete = cache
            .current_snapshot()
            .principals
            .get(&(principal_id as u64))
            .expect("the principal itself is not deleted")
            .limits;
        assert_eq!(
            published_after_delete, None,
            "removing the limit must make the principal unlimited, not limited to zero"
        );
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn put_limits_rejects_an_empty_body() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "empty-limits-principal-");
        let principal_id = make_principal(&ctx, &unique_name("empty-limits-principal")).await;
        let err = put_limits(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(PutLimits {
                requests_per_min: None,
                tokens_per_min: None,
            }),
        )
        .await
        .expect_err("a body with neither dimension set must be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn deleting_a_limit_that_does_not_exist_is_not_found() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new().track_prefix("principals", "name", "no-limit-principal-");
        let principal_id = make_principal(&ctx, &unique_name("no-limit-principal")).await;
        let err = delete_limits(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
        )
        .await
        .expect_err("no row to delete");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    /// `POST /limits/reconcile` is gated by the same proxy token as
    /// `/snapshot` and `/usage`, and its aggregation is exactly
    /// `control::reconcile::ReconcileState::report` — this exercises it
    /// through the actual route handler rather than only the pure function,
    /// proving the wire wrapper does not lose or misroute anything.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn the_reconcile_route_is_gated_and_answers_with_a_computed_share() {
        let (ctx, _cache) = test_ctx().await;

        let unauthorised = post_reconcile(
            State(ctx.clone()),
            HeaderMap::new(),
            Json(ReconcileRequest {
                replica_id: "r1".into(),
                counts: vec![],
            }),
        )
        .await;
        assert!(unauthorised.is_err());

        let resp = post_reconcile(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(ReconcileRequest {
                replica_id: "r1".into(),
                counts: vec![ReconcileCountWire {
                    principal_id: 999,
                    requests: 10,
                    tokens: 100,
                }],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.allowances.len(), 1);
        assert_eq!(resp.0.allowances[0].principal_id, 999);
        // The only replica that has ever reported for this principal gets
        // the full share — same property `control::reconcile`'s own tests
        // pin, exercised here through the HTTP-facing wrapper.
        assert_eq!(resp.0.allowances[0].requests_share, 1.0);
        assert_eq!(resp.0.allowances[0].tokens_share, 1.0);
    }

    // --- Budgets (P3) ---------------------------------------------------

    /// Same write-path proof `put_and_delete_limits_reach_the_published_snapshot`
    /// makes for `limits`: `put_budget` reaches the published snapshot,
    /// `delete_budget` removes it again, and the principal ends up
    /// unlimited afterward — not limited to zero.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn put_and_delete_budget_reach_the_published_snapshot() {
        let (ctx, cache) = test_ctx().await;
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "budget-route-principal-");
        let principal_id = make_principal(&ctx, &unique_name("budget-route-principal")).await;

        put_budget(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(PutBudget {
                tokens_total: 500,
                window: "daily".into(),
            }),
        )
        .await
        .unwrap();

        let published = cache
            .current_snapshot()
            .principals
            .get(&(principal_id as u64))
            .expect("the principal must be in the published snapshot")
            .budget;
        assert_eq!(
            published,
            Some(crate::snapshot::Budget {
                tokens_total: 500,
                tokens_used: 0,
            })
        );

        let listed = list_budgets(State(ctx.clone()), RequireRead::default())
            .await
            .unwrap();
        assert!(listed
            .0
            .iter()
            .any(|b| b.principal_id == principal_id && b.tokens_total == 500));

        delete_budget(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
        )
        .await
        .unwrap();
        let published_after_delete = cache
            .current_snapshot()
            .principals
            .get(&(principal_id as u64))
            .expect("the principal itself is not deleted")
            .budget;
        assert_eq!(
            published_after_delete, None,
            "removing the budget must make the principal unlimited, not limited to zero"
        );
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn put_budget_rejects_a_bad_window_or_non_positive_total() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "bad-budget-principal-");
        let principal_id = make_principal(&ctx, &unique_name("bad-budget-principal")).await;

        let err = put_budget(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(PutBudget {
                tokens_total: 100,
                window: "fortnightly".into(),
            }),
        )
        .await
        .expect_err("an unrecognised window must be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = put_budget(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(PutBudget {
                tokens_total: 0,
                window: "daily".into(),
            }),
        )
        .await
        .expect_err("a non-positive tokens_total must be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn deleting_a_budget_that_does_not_exist_is_not_found() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup =
            TestCleanup::new().track_prefix("principals", "name", "no-budget-principal-");
        let principal_id = make_principal(&ctx, &unique_name("no-budget-principal")).await;
        let err = delete_budget(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
        )
        .await
        .expect_err("no row to delete");
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    /// The other half of P3's usage flow: `POST /usage` does not just write
    /// `usage_events`, it also folds accepted rows into `budgets.tokens_used`
    /// — this is what lets a completed request's *real* token count, not the
    /// request path's estimate, push a principal toward (or over) its
    /// budget.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_usage_report_increments_the_matching_principals_budget() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "budget-usage-principal-")
            .track_prefix("models", "name", "budget-usage-model-");
        let principal_id = make_principal(&ctx, &unique_name("budget-usage-principal")).await;
        let model_name = unique_name("budget-usage-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: model_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        put_budget(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(principal_id),
            Json(PutBudget {
                tokens_total: 1000,
                window: "monthly".into(),
            }),
        )
        .await
        .unwrap();

        // usage_event(..) below is prompt_tokens: 10, completion_tokens: 5 —
        // 15 tokens per event, two events, 30 total.
        let resp = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![
                    usage_event(principal_id, &model_name),
                    usage_event(principal_id, &model_name),
                ],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.accepted, 2);

        let tokens_used: i64 =
            sqlx::query_scalar("SELECT tokens_used FROM budgets WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_one(&ctx.pool)
                .await
                .unwrap();
        assert_eq!(tokens_used, 30);
    }

    /// A usage report for a principal with no configured budget must not
    /// error or create a row — `apply_usage_to_budgets`'s `UPDATE` simply
    /// matches zero rows, same as any other principal with nothing
    /// configured.
    /// A backend declaring a service-account credential must be rejected at
    /// write time if the credential is not one. Left to snapshot build, the
    /// mistake becomes a backend that looks configured in the API, is missing
    /// from routing, and says so only in the control plane's log.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_service_account_credential_is_validated_when_the_backend_is_created() {
        let (ctx, _cache) = test_ctx().await;
        let model_name = unique_name("gcp-validate");
        let (_, model) = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: model_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        let model_id = model.0["id"].as_i64().unwrap();

        let vertex = |key: Option<String>, kind: Option<&str>| {
            NewBackend {
            api_base: "https://europe-west1-aiplatform.googleapis.com/v1/projects/p/locations/europe-west1/endpoints/openapi".into(),
            upstream_model: None,
            upstream_api_key: key,
            protocol: None,
            auth_header: None,
            auth_scheme: None,
            default_max_tokens: None,
            credential_kind: kind.map(str::to_string),
        }
        };

        // A plain API key is not a service-account key file.
        let err = post_backend(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(model_id),
            Json(vertex(
                Some("sk-not-a-key-file".into()),
                Some("gcp_service_account"),
            )),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // Nor is no credential at all.
        let err = post_backend(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(model_id),
            Json(vertex(None, Some("gcp_service_account"))),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // An unknown kind names the valid ones rather than reaching the
        // column's CHECK constraint as a Postgres error.
        let err = post_backend(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(model_id),
            Json(vertex(Some("x".into()), Some("iam_role"))),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // A real key file shape is accepted, and the kind is echoed back so an
        // operator can see which credential path a backend is on.
        let key_file = serde_json::json!({
            "type": "service_account",
            "client_email": "vertex@example.iam.gserviceaccount.com",
            "private_key": "-----BEGIN PRIVATE KEY-----\nnot-a-real-key\n-----END PRIVATE KEY-----\n",
        })
        .to_string();
        let (status, created) = post_backend(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(model_id),
            Json(vertex(Some(key_file), Some("gcp_service_account"))),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created.0["credential_kind"], "gcp_service_account");

        // And the default stays `static`, so every existing caller is
        // unaffected by the field existing.
        let (_, plain) = post_backend(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Path(model_id),
            Json(NewBackend::openai("http://plain:8000/v1", None)),
        )
        .await
        .unwrap();
        assert_eq!(plain.0["credential_kind"], "static");
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_usage_report_for_an_unbudgeted_principal_creates_no_budget_row() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "no-budget-usage-principal-")
            .track_prefix("models", "name", "no-budget-usage-model-");
        let principal_id = make_principal(&ctx, &unique_name("no-budget-usage-principal")).await;
        let model_name = unique_name("no-budget-usage-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite::default(),
            Json(NewModel {
                name: model_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();

        let resp = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![usage_event(principal_id, &model_name)],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.accepted, 1);

        let row: Option<i64> =
            sqlx::query_scalar("SELECT tokens_used FROM budgets WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_optional(&ctx.pool)
                .await
                .unwrap();
        assert!(
            row.is_none(),
            "no budget row must be created out of thin air"
        );
    }
}

// --- Prompt classes (semantic routing) --------------------------------------

#[derive(Serialize)]
struct PromptClassView {
    id: i64,
    name: String,
    description: String,
    tier: String,
    min_margin: Option<f32>,
    refines: Vec<String>,
    examples: i64,
    /// Whether the last snapshot build produced a centroid for this class. A
    /// class with examples but no centroid means the control plane has no
    /// classifier model — surfaced because otherwise the only symptom is a rule
    /// that silently never matches.
    routable: bool,
}

async fn list_prompt_classes(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<PromptClassView>>, ApiError> {
    let rows: Vec<(i64, String, String, String, Option<f32>)> = sqlx::query_as(
        "SELECT id, name, description, tier, min_margin FROM prompt_classes ORDER BY name",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing prompt classes", &e))?;

    let counts: Vec<(i64, i64)> =
        sqlx::query_as("SELECT class_id, count(*) FROM prompt_class_examples GROUP BY class_id")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("counting class examples", &e))?;
    let refines: Vec<(i64, String)> =
        sqlx::query_as("SELECT class_id, refines FROM prompt_class_refines")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing class refinements", &e))?;

    let snapshot = ctx.cache.current_snapshot();
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, name, description, tier, min_margin)| PromptClassView {
                    routable: snapshot
                        .prompt_classes
                        .iter()
                        .any(|c| c.name == name && !c.centroid.is_empty()),
                    examples: counts
                        .iter()
                        .find(|(cid, _)| *cid == id)
                        .map(|(_, n)| *n)
                        .unwrap_or(0),
                    refines: refines
                        .iter()
                        .filter(|(cid, _)| *cid == id)
                        .map(|(_, r)| r.clone())
                        .collect(),
                    id,
                    name,
                    description,
                    tier,
                    min_margin,
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewPromptClass {
    name: String,
    #[serde(default)]
    description: String,
    /// `fast` (default) or `refined`.
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    min_margin: Option<f32>,
    /// Fast-tier class names this one competes with. Only meaningful for a
    /// `refined` class, and the thing that decides whether the transformer is
    /// ever loaded at all.
    #[serde(default)]
    refines: Vec<String>,
    /// Example prompts. Seeding from real traffic beats writing tidy
    /// one-liners — measured, see docs/classifier.md.
    #[serde(default)]
    examples: Vec<String>,
}

async fn post_prompt_class(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewPromptClass>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must not be empty".to_string(),
        ));
    }
    let tier = body.tier.unwrap_or_else(|| "fast".into());
    if tier != "fast" && tier != "refined" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("tier {tier:?} must be \"fast\" or \"refined\""),
        ));
    }
    if body.min_margin.is_some_and(|m| !(0.0..=2.0).contains(&m)) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "min_margin must be between 0 and 2".to_string(),
        ));
    }
    // A refined class that refines nothing can never be reached: escalation is
    // keyed entirely on the fast-tier class it names. Better a 400 now than a
    // class that looks configured and never matches.
    if tier == "refined" && body.refines.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "a refined class must name at least one fast-tier class in `refines`, or nothing \
             will ever escalate to it"
                .to_string(),
        ));
    }

    let mut tx = ctx
        .pool
        .begin()
        .await
        .map_err(|e| db_error("prompt class creation", &e))?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO prompt_classes (name, description, tier, min_margin)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(&name)
    .bind(&body.description)
    .bind(&tier)
    .bind(body.min_margin)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        // A name collision is the caller's mistake, not a server fault, and
        // "see server logs" for something the caller can fix by picking another
        // name is a poor answer.
        if e.as_database_error()
            .is_some_and(|d| d.is_unique_violation())
        {
            api_error(
                StatusCode::CONFLICT,
                format!("a prompt class named {name:?} already exists"),
            )
        } else {
            db_error("prompt class creation", &e)
        }
    })?;

    for r in &body.refines {
        sqlx::query("INSERT INTO prompt_class_refines (class_id, refines) VALUES ($1, $2)")
            .bind(id)
            .bind(r)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_error("recording class refinement", &e))?;
    }
    for p in &body.examples {
        sqlx::query("INSERT INTO prompt_class_examples (class_id, prompt) VALUES ($1, $2)")
            .bind(id)
            .bind(p)
            .execute(&mut *tx)
            .await
            .map_err(|e| db_error("recording class example", &e))?;
    }
    tx.commit()
        .await
        .map_err(|e| db_error("prompt class creation", &e))?;
    refresh(&ctx).await;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": id, "name": name, "tier": tier,
            "examples": body.examples.len(), "refines": body.refines,
        })),
    ))
}

#[derive(Deserialize)]
struct NewExample {
    prompt: String,
}

async fn post_prompt_class_example(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(class_id): Path<i64>,
    Json(body): Json<NewExample>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO prompt_class_examples (class_id, prompt) VALUES ($1, $2) RETURNING id",
    )
    .bind(class_id)
    .bind(&body.prompt)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if e.as_database_error()
            .is_some_and(|d| d.is_foreign_key_violation())
        {
            api_error(
                StatusCode::NOT_FOUND,
                format!("no prompt class with id {class_id}"),
            )
        } else {
            db_error("recording class example", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((StatusCode::CREATED, Json(serde_json::json!({"id": id}))))
}

async fn delete_prompt_class(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM prompt_classes WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("prompt class deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no prompt class with id {id}"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Report how well an operator's own classes separate.
///
/// Two diagnostics, because they fail differently. **Centroid similarity** is
/// cheap and predicts trouble before any traffic flows: a pair above ~0.8 is one
/// region of the space with two names, and no threshold separates them.
/// **Leave-one-out precision and recall** is the empirical one: each example is
/// scored against centroids built from the *other* examples of its class, which
/// is exactly how a real prompt will be scored.
///
/// The measurements this feature is built on found that class *definition*, not
/// class count, decides quality — and that the failure is invisible without a
/// report like this, because a class at 20% precision looks identical from the
/// outside to one at 98%.
async fn evaluate_prompt_classes(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rows: Vec<(i64, String, String, Option<f32>)> =
        sqlx::query_as("SELECT id, name, tier, min_margin FROM prompt_classes ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing prompt classes", &e))?;
    if rows.is_empty() {
        return Ok(Json(
            serde_json::json!({"classes": [], "note": "no prompt classes are defined"}),
        ));
    }

    let Some(embedder) = crate::control::build::prompt_class_embedder() else {
        return Ok(Json(serde_json::json!({
            "classes": [],
            "note": "this control plane has no --classifier-model, so it cannot embed examples \
                     and cannot report on them",
        })));
    };

    let examples: Vec<(i64, String)> =
        sqlx::query_as("SELECT class_id, prompt FROM prompt_class_examples ORDER BY id")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing class examples", &e))?;

    // Embedded per tier: the two spaces are not comparable, so a fast class and
    // a refined one are never scored against each other.
    struct Embedded {
        name: String,
        tier: String,
        min_margin: Option<f32>,
        prompts: Vec<String>,
        vectors: Vec<Vec<f32>>,
    }
    let mut sets: Vec<Embedded> = Vec::new();
    for (id, name, tier, min_margin) in rows {
        let prompts: Vec<String> = examples
            .iter()
            .filter(|(cid, _)| *cid == id)
            .map(|(_, p)| p.clone())
            .collect();
        if prompts.is_empty() {
            continue;
        }
        let vectors = match tier.as_str() {
            "refined" => embedder.refined(&prompts),
            _ => embedder.fast(&prompts),
        };
        let Some(vectors) = vectors else { continue };
        sets.push(Embedded {
            name,
            tier,
            min_margin,
            prompts,
            vectors,
        });
    }
    if sets.is_empty() {
        return Ok(Json(serde_json::json!({
            "classes": [],
            "note": "no class has example prompts that could be embedded",
        })));
    }

    // Leave-one-out. Every example is classified against centroids that exclude
    // it; scoring against a centroid that contains the example inflates every
    // number, and at these sample sizes enough to change which class looks
    // usable.
    let mut true_positive: HashMap<&str, usize> = HashMap::new();
    let mut predicted: HashMap<&str, usize> = HashMap::new();
    let mut margins: HashMap<&str, Vec<f32>> = HashMap::new();
    let mut confusions: Vec<serde_json::Value> = Vec::new();

    for (i, set) in sets.iter().enumerate() {
        for (held, vector) in set.vectors.iter().enumerate() {
            let mut scored: Vec<(&str, f32)> = sets
                .iter()
                .enumerate()
                .filter(|(_, other)| other.tier == set.tier)
                .filter_map(|(j, other)| {
                    let subset: Vec<Vec<f32>> = other
                        .vectors
                        .iter()
                        .enumerate()
                        .filter(|(k, _)| !(i == j && *k == held))
                        .map(|(_, v)| v.clone())
                        .collect();
                    crate::vector::centroid(&subset)
                        .map(|c| (other.name.as_str(), cosine(vector, &c)))
                })
                .collect();
            if scored.is_empty() {
                continue;
            }
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let winner = scored[0].0;
            let margin = scored[0].1 - scored.get(1).map(|s| s.1).unwrap_or(0.0);
            *predicted.entry(winner).or_default() += 1;
            margins.entry(set.name.as_str()).or_default().push(margin);
            if winner == set.name {
                *true_positive.entry(winner).or_default() += 1;
            } else if confusions.len() < 20 {
                // The example itself, truncated: an operator fixing a weak
                // class needs to see *which* prompt went the wrong way, not
                // only that one did.
                confusions.push(serde_json::json!({
                    "actual": set.name,
                    "predicted": winner,
                    "example": set.prompts.get(held).map(|p| p.chars().take(120).collect::<String>()),
                }));
            }
        }
    }

    let mut report = Vec::new();
    for set in &sets {
        let name = set.name.as_str();
        let tp = *true_positive.get(name).unwrap_or(&0) as f64;
        let pred = *predicted.get(name).unwrap_or(&0) as f64;
        let support = set.vectors.len() as f64;
        let ms = margins.get(name).cloned().unwrap_or_default();
        let mean_margin = if ms.is_empty() {
            0.0
        } else {
            ms.iter().sum::<f32>() / ms.len() as f32
        };
        let worst_margin = ms.iter().copied().fold(f32::INFINITY, f32::min);

        // Nearest neighbour in the same tier, from full centroids.
        let mine = crate::vector::centroid(&set.vectors);
        let mut nearest: Vec<(String, f32)> = sets
            .iter()
            .filter(|o| o.name != set.name && o.tier == set.tier)
            .filter_map(|o| {
                let c = crate::vector::centroid(&o.vectors)?;
                Some((o.name.clone(), cosine(mine.as_deref().unwrap_or(&[]), &c)))
            })
            .collect();
        nearest.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        nearest.truncate(3);

        let precision = if pred > 0.0 { tp / pred } else { 0.0 };
        let recall = if support > 0.0 { tp / support } else { 0.0 };
        report.push(serde_json::json!({
            "class": name,
            "tier": set.tier,
            "examples": support as usize,
            "precision": precision,
            "recall": recall,
            "mean_margin": mean_margin,
            "worst_margin": if worst_margin.is_finite() { worst_margin } else { 0.0 },
            "min_margin": set.min_margin,
            "nearest": nearest,
            "collides": nearest.first().map(|(_, s)| *s > 0.8).unwrap_or(false),
            // The two things an operator should act on, said plainly rather
            // than left to be inferred from four numbers.
            "verdict": if nearest.first().map(|(_, s)| *s > 0.8).unwrap_or(false) {
                "collides with another class; merge or redefine them"
            } else if precision >= 0.85 {
                "good"
            } else if precision >= 0.70 {
                "usable; consider more examples or a higher min_margin"
            } else {
                "weak; this class will misroute"
            },
        }));
    }
    Ok(Json(
        serde_json::json!({ "classes": report, "confusions": confusions }),
    ))
}

// --- Deployment-wide fallback model -----------------------------------------

async fn get_fallback_model(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM models WHERE is_fallback LIMIT 1")
            .fetch_optional(&ctx.pool)
            .await
            .map_err(|e| db_error("reading the fallback model", &e))?;
    Ok(Json(match row {
        Some((id, name)) => serde_json::json!({"id": id, "name": name}),
        None => serde_json::json!({"id": null, "name": null}),
    }))
}

#[derive(Deserialize)]
struct FallbackModel {
    /// `null` clears it, leaving no deployment-wide last resort.
    model_id: Option<i64>,
}

/// Set (or clear) the model every routing chain falls back to.
///
/// One statement pair in a transaction, because "at most one model is the
/// fallback" is enforced by a partial unique index: clearing before setting is
/// not tidiness, it is what stops the insert failing against the old value.
async fn put_fallback_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<FallbackModel>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut tx = ctx
        .pool
        .begin()
        .await
        .map_err(|e| db_error("setting the fallback model", &e))?;
    sqlx::query("UPDATE models SET is_fallback = false WHERE is_fallback")
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("clearing the previous fallback model", &e))?;

    let mut name = None;
    if let Some(id) = body.model_id {
        let updated: Option<String> =
            sqlx::query_scalar("UPDATE models SET is_fallback = true WHERE id = $1 RETURNING name")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| db_error("setting the fallback model", &e))?;
        let Some(found) = updated else {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                format!("no model with id {id}"),
            ));
        };
        name = Some(found);
    }
    tx.commit()
        .await
        .map_err(|e| db_error("setting the fallback model", &e))?;
    refresh(&ctx).await;
    Ok(Json(
        serde_json::json!({"model_id": body.model_id, "name": name}),
    ))
}
