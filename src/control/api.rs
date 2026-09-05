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
    /// Flags this process was started with, for `GET /admin/config`.
    deployment: Arc<Deployment>,
    /// Outbound notifications, or a disabled sender when none is configured.
    webhook: Arc<crate::webhook::WebhookSender>,
    started_at: std::time::Instant,
    /// Whether `serve` bound with TLS — the one bit `login`/`logout` need to
    /// decide whether to set `Secure` on the session cookie. `Secure`
    /// unconditionally would make the cookie silently never round-trip over
    /// the plain-HTTP dev path this crate already tolerates for `/snapshot`;
    /// omitting it whenever TLS *is* on would ship a session cookie plain
    /// HTTP could read.
    tls_enabled: bool,
    /// The latest health each proxy reported. In memory and not a table:
    /// health is a statement about *now*, and a row saying a backend was up two
    /// hours ago is history nobody asked for. See `crate::health_report`.
    fleet: Arc<crate::health_report::store::Fleet>,
    /// The `FastllmProxy` managing this process, if one does.
    ///
    /// `None` for every other way of running this — File mode, Helm, a
    /// laptop — and that is what makes the UI's deployment screen appear only
    /// where it can do something. See `control::k8s`.
    operator: Option<Arc<crate::control::k8s::Operator>>,
}

/// Every admin route fails the same way `post_key` does: a status plus a JSON
/// body that says what was wrong. `/admin/*` is authenticated (`require_session`)
/// and authorised (`check_permission`) and has a management UI (`control::ui`),
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
struct ProviderView {
    id: i64,
    name: String,
    /// `static`, `cloud` or `dynamic`. Only `dynamic` providers are ever
    /// removed automatically; the other two are here because a human put them
    /// here, and absence is not evidence that the human changed their mind.
    kind: String,
    api_base: String,
    protocol: String,
    auth_header: String,
    /// Whether a credential is configured — never the credential itself, in
    /// either plaintext or ciphertext. `upstream_api_key` is the one secret in
    /// this schema that cannot be reduced to a hash (the proxy has to present
    /// it upstream), so the only safe thing to say about it here is whether it
    /// is set.
    has_upstream_api_key: bool,
    /// Which catalogue entry this came from, absent for a hand-typed address.
    catalogue_key: Option<String>,
    /// How many provider models this provider serves. The count rather than
    /// the models themselves: `GET /admin/provider-models` already carries those, and a
    /// provider fronting a few hundred models would make this response the
    /// wrong shape for the question it answers.
    model_count: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderRegistration {
    /// The address **proxies** should dial, not one inferred from local
    /// discovery. An agent that finds a container on `172.17.0.2` and
    /// registers that hands the proxies an address they cannot reach.
    api_base: String,
    /// Which host is vouching for it. One principal per node scopes a
    /// compromised host to its own providers.
    node: String,
    /// Metadata only — see `registry_agent::served_models` for why nothing
    /// depends on it.
    #[serde(default)]
    engine: Option<String>,
    /// How long this registration is good for. The agent refreshes well
    /// inside it; a lapse is what starts the provider degrading.
    ttl_seconds: i64,
}

/// `POST /admin/providers/register` — a host says what address it is serving on.
///
/// Deliberately does *not* take a model list. The control plane enumerates the
/// provider itself, so discovery and reachability are the same test and a model
/// the proxies cannot dial is never registered. See
/// `.procoder/adr/0003-the-control-plane-enumerates-a-providers-models.md`.
async fn post_provider_register(
    State(ctx): State<Ctx>,
    _perm: RequireProviderRegister,
    Json(body): Json<ProviderRegistration>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let api_base = body.api_base.trim().trim_end_matches('/').to_string();
    if !(api_base.starts_with("http://") || api_base.starts_with("https://")) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("api_base {api_base:?} must start with http:// or https://"),
        ));
    }
    if body.ttl_seconds <= 0 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "ttl_seconds must be greater than zero".to_string(),
        ));
    }
    if body.node.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "node must name the host registering this provider".to_string(),
        ));
    }

    let id = crate::control::registry_agent::register(
        &ctx.pool,
        &api_base,
        body.node.trim(),
        body.engine.as_deref(),
        body.ttl_seconds,
    )
    .await
    .map_err(|e| db_error("registering a provider", &e))?;

    // A provider registered by hand keeps the kind it was given: registration
    // must never quietly convert a static provider into one that can expire.
    let kind: String = sqlx::query_scalar("SELECT kind FROM providers WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.pool)
        .await
        .map_err(|e| db_error("reading the provider back", &e))?;

    refresh(&ctx).await;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "id": id,
            "api_base": api_base,
            "kind": kind,
            "leased": kind == "dynamic",
        })),
    ))
}

#[derive(Serialize)]
struct CatalogueEntry {
    key: String,
    display_name: String,
    /// May contain `<placeholders>` a human must fill in. Bedrock and Vertex
    /// both encode a region; prefilling them as-is would hand the operator an
    /// address that cannot resolve, and pretending otherwise would be worse
    /// than saying so.
    base_url: String,
    protocol: String,
    auth_header: String,
    auth_scheme: Option<String>,
    notes: Option<String>,
}

/// `GET /admin/provider-catalogue` — known providers and how to reach them.
///
/// Not a permission list and not a limit. Anything speaking the OpenAI API
/// works whether or not it is here; this exists so an operator does not have
/// to go and find the base URL and the header a vendor wants its key in.
///
/// It covers the entries `docs/providers.md` actually documents an endpoint
/// for. The page names about a hundred providers and gives a host for
/// thirty-odd; seeding the rest would mean inventing their base URLs.
async fn list_provider_catalogue(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<CatalogueEntry>>, ApiError> {
    type Row = (
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT key, display_name, base_url, protocol, auth_header, auth_scheme, notes \
         FROM provider_catalogue ORDER BY display_name",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing the provider catalogue", &e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(key, display_name, base_url, protocol, auth_header, auth_scheme, notes)| {
                    CatalogueEntry {
                        key,
                        display_name,
                        base_url,
                        protocol,
                        auth_header,
                        auth_scheme,
                        notes,
                    }
                },
            )
            .collect(),
    ))
}

/// `DELETE /admin/providers/{id}` — remove a provider that serves nothing.
///
/// Refuses while provider models remain on it, rather than cascading. A
/// cascade here would delete provider models, and with them frontend model
/// targets and — before migration 0031 — the usage those models were billed
/// for. Deleting a provider is a tidying-up operation; it should not be able
/// to take routing and history with it by accident.
///
/// The operator deletes the models first, which is the order that makes what
/// is being lost visible one step at a time.
async fn delete_provider(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let models: Vec<String> =
        sqlx::query_scalar("SELECT name FROM provider_models WHERE provider_id = $1 ORDER BY name")
            .bind(id)
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("checking a provider's models", &e))?;
    if !models.is_empty() {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "provider {id} still serves {n} model(s): {models:?}. Delete them first — \
                 removing the provider would take their routing targets with them",
                n = models.len()
            ),
        ));
    }
    let done = sqlx::query("DELETE FROM providers WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("deleting a provider", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no provider with id {id}; GET /admin/providers lists the ids that exist"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The Providers screen used to invent this by grouping backends on their
/// `api_base` at render time, which meant a provider could not be named,
/// counted, or referred to by anything else. Since migration 0029 it is a row.
async fn list_providers(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<ProviderView>>, ApiError> {
    type Row = (
        i64,
        String,
        String,
        String,
        String,
        String,
        bool,
        Option<String>,
        i64,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT p.id, p.name, p.kind, p.api_base, p.protocol, p.auth_header, \
             p.upstream_api_key IS NOT NULL, p.catalogue_key, \
             (SELECT count(*) FROM provider_models m WHERE m.provider_id = p.id) \
         FROM providers p ORDER BY p.name",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing providers", &e))?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(
                    id,
                    name,
                    kind,
                    api_base,
                    protocol,
                    auth_header,
                    has_upstream_api_key,
                    catalogue_key,
                    model_count,
                )| ProviderView {
                    id,
                    name,
                    kind,
                    api_base,
                    protocol,
                    auth_header,
                    has_upstream_api_key,
                    catalogue_key,
                    model_count,
                },
            )
            .collect(),
    ))
}

/// A model's link to its provider, kept in the shape the UI already reads.
///
/// Since migration 0029 a provider model has exactly one provider, so this is
/// a view over the join rather than a row of its own — `id` is the **model's**
/// id, because the model *is* the link. `DELETE /admin/backends/{id}` detaches
/// that model from its provider and takes the same id.
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
    /// Micro-units per million tokens. `None` is unpriced — usage is still
    /// recorded, cost is left NULL rather than assumed zero.
    input_price_per_mtok: Option<i64>,
    output_price_per_mtok: Option<i64>,
    /// `None` is caching off.
    cache_ttl_seconds: Option<i32>,
    /// Tokens this model accepts, or absent when nobody has declared it.
    /// Absent is a third state, not zero — routing demotes a model only when
    /// the figure is known and too small.
    context_length: Option<i64>,
    /// The provider serving this model, absent when none is configured.
    ///
    /// Absent is a real state and not an error: a model can outlive its
    /// provider, and before migration 0029 the same condition was "a model
    /// with no backends". It is not routable, and the UI shows it as needing
    /// attention rather than hiding it.
    provider_id: Option<i64>,
    provider_name: Option<String>,
    /// Zero or one entry, never more. Kept as a list so existing clients and
    /// the UI keep parsing; the one-provider rule is enforced by the schema.
    backends: Vec<BackendView>,
}

async fn list_models(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<ModelView>>, ApiError> {
    // `upstream_api_key IS NOT NULL` rather than the column: this query must
    // not be able to return a credential even by accident, so the ciphertext
    // never leaves Postgres on this path at all.
    type ModelRow = (
        i64,
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i32>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<bool>,
        Option<String>,
        Option<String>,
        Option<i32>,
    );
    let models: Vec<ModelRow> = sqlx::query_as(
        "SELECT m.id, m.name, m.description, m.input_price_per_mtok, \
             m.output_price_per_mtok, m.cache_ttl_seconds, m.context_length, \
             p.id, p.name, p.api_base, m.upstream_model, \
             p.upstream_api_key IS NOT NULL, p.protocol, p.auth_header, m.default_max_tokens \
         FROM provider_models m LEFT JOIN providers p ON p.id = m.provider_id ORDER BY m.name",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing models", &e))?;

    Ok(Json(
        models
            .into_iter()
            .map(
                |(
                    id,
                    name,
                    description,
                    input_price_per_mtok,
                    output_price_per_mtok,
                    cache_ttl_seconds,
                    context_length,
                    provider_id,
                    provider_name,
                    api_base,
                    upstream_model,
                    has_key,
                    protocol,
                    auth_header,
                    default_max_tokens,
                )| {
                    // Every provider column arrives together or not at all —
                    // they come from one LEFT JOIN row — so one of them
                    // deciding is enough, and `api_base` is the one without
                    // which nothing is routable.
                    let backends = match api_base {
                        Some(api_base) => vec![BackendView {
                            id,
                            api_base,
                            upstream_model: upstream_model.unwrap_or_else(|| name.clone()),
                            has_upstream_api_key: has_key.unwrap_or(false),
                            protocol: protocol.unwrap_or_else(|| "openai".into()),
                            auth_header: auth_header.unwrap_or_else(|| "authorization".into()),
                            default_max_tokens,
                        }],
                        None => Vec::new(),
                    };
                    ModelView {
                        id,
                        name,
                        description,
                        input_price_per_mtok,
                        output_price_per_mtok,
                        cache_ttl_seconds,
                        context_length,
                        provider_id,
                        provider_name,
                        backends,
                    }
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewModel {
    name: String,
    #[serde(default)]
    description: String,
    /// Price per *million* tokens, in micro-units — the unit every provider
    /// publishes, and an integer so the arithmetic is exact.
    ///
    /// Unset leaves the model unpriced: usage is still recorded, but cost is
    /// left NULL rather than assumed zero, so unpriced is visible instead of
    /// looking free.
    #[serde(default)]
    input_price_per_mtok: Option<i64>,
    #[serde(default)]
    output_price_per_mtok: Option<i64>,
    /// Seconds an identical request may be answered from cache. Unset or 0 is
    /// off, which is the default: caching changes semantics, since two
    /// identical requests at `temperature > 0` are supposed to be able to
    /// differ.
    #[serde(default)]
    cache_ttl_seconds: Option<i32>,
    /// Declared context window, used to demote a model whose window is too
    /// small for the request rather than letting the upstream refuse it.
    ///
    /// Settable here as well as by PATCH because it was PATCH-only, and a
    /// caller who sent it to this route got no error and no context length —
    /// the field was simply dropped.
    #[serde(default)]
    context_length: Option<i32>,
}

/// Accept only a policy this build knows, and say which ones those are.
///
/// `Ok(None)` for absent — meaning the deployment default — which is a
/// different thing from an empty string, and both spell "unset" here so a UI
/// clearing the field does not have to send `null` specifically.
fn validated_policy(policy: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(raw) = policy.map(str::trim).filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    match crate::router::Policy::parse(raw) {
        Some(p) => Ok(Some(p.as_str().to_string())),
        None => Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "unknown policy {raw:?}; expected one of cache-affinity, least-loaded, \
                 round-robin, lowest-latency, or omit it to use the deployment default"
            ),
        )),
    }
}

/// `(id, name, url, transport, description, auth_header, auth_scheme, enabled,
/// credential_set)` — a row shape, named because clippy is right that nine
/// anonymous tuple elements is not a type anyone can read.
type McpServerRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    bool,
    bool,
);

/// `(id, name, url, description, protocol_version, auth_header, auth_scheme,
/// enabled, credential_set)`.
type A2aAgentRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    bool,
    bool,
);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewA2aAgent {
    name: String,
    url: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    protocol_version: Option<String>,
    #[serde(default)]
    auth_header: Option<String>,
    #[serde(default)]
    auth_scheme: Option<String>,
    #[serde(default)]
    upstream_api_key: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// `GET /admin/a2a-agents`. Reports whether a credential is set, never what.
async fn list_a2a_agents(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rows: Vec<A2aAgentRow> = sqlx::query_as(
        "SELECT id, name, url, description, protocol_version, auth_header, auth_scheme, enabled,
                upstream_api_key IS NOT NULL
           FROM a2a_agents ORDER BY name",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("a2a agent list", &e))?;

    let data: Vec<serde_json::Value> = rows
        .into_iter()
        .map(
            |(id, name, url, description, version, header, scheme, enabled, cred)| {
                serde_json::json!({
                    "id": id, "name": name, "url": url, "description": description,
                    "protocol_version": version, "auth_header": header,
                    "auth_scheme": scheme, "enabled": enabled, "credential_set": cred,
                })
            },
        )
        .collect();
    Ok(Json(serde_json::json!({ "data": data })))
}

/// `POST /admin/a2a-agents`.
async fn post_a2a_agent(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewA2aAgent>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if body.name.is_empty()
        || !body
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must be alphanumeric with - or _: it is a URL path segment",
        ));
    }
    let version = body.protocol_version.unwrap_or_else(|| "0.3".to_string());
    if version != "0.3" && version != "1.0" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "protocol_version must be \"0.3\" or \"1.0\", pinned rather than inferred: a \
             guessed version means the agent card and the responses that follow it disagree",
        ));
    }
    let encrypted = body
        .upstream_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| crate::control::secrets::encrypt(&ctx.key, k))
        .transpose()
        .map_err(|e| {
            tracing::error!(error = %e, "encrypting an A2A agent credential failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not encrypt upstream_api_key; the agent was not created",
            )
        })?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO a2a_agents
           (name, url, description, protocol_version, auth_header, auth_scheme,
            upstream_api_key, enabled)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(&body.name)
    .bind(&body.url)
    .bind(&body.description)
    .bind(&version)
    .bind(body.auth_header.as_deref().unwrap_or("authorization"))
    .bind(match body.auth_scheme.as_deref() {
        None => Some("Bearer"),
        Some("") => None,
        Some(scheme) => Some(scheme),
    })
    .bind(encrypted)
    .bind(body.enabled.unwrap_or(true))
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!("an agent named {:?} already exists", body.name),
            )
        } else {
            db_error("a2a agent creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": body.name })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchA2aAgent {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    protocol_version: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    /// Absent leaves the credential alone; `""` clears it.
    #[serde(default)]
    upstream_api_key: Option<String>,
}

/// `PATCH /admin/a2a-agents/{id}`.
async fn patch_a2a_agent(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
    Json(body): Json<PatchA2aAgent>,
) -> Result<StatusCode, ApiError> {
    if let Some(v) = body.protocol_version.as_deref() {
        if v != "0.3" && v != "1.0" {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "protocol_version must be \"0.3\" or \"1.0\"",
            ));
        }
    }
    let encrypted = match body.upstream_api_key.as_deref().map(str::trim) {
        Some("") => Some(None),
        Some(k) => Some(Some(
            crate::control::secrets::encrypt(&ctx.key, k).map_err(|e| {
                tracing::error!(error = %e, "encrypting an A2A agent credential failed");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not encrypt upstream_api_key; nothing was changed",
                )
            })?,
        )),
        None => None,
    };
    let result = sqlx::query(
        "UPDATE a2a_agents
            SET url = COALESCE($2, url),
                description = COALESCE($3, description),
                protocol_version = COALESCE($4, protocol_version),
                enabled = COALESCE($5, enabled),
                upstream_api_key = CASE WHEN $6 THEN $7 ELSE upstream_api_key END,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.url.as_deref())
    .bind(body.description.as_deref())
    .bind(body.protocol_version.as_deref())
    .bind(body.enabled)
    .bind(encrypted.is_some())
    .bind(encrypted.flatten())
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("a2a agent update", &e))?;
    if result.rows_affected() == 0 {
        return Err(api_error(StatusCode::NOT_FOUND, "no such agent"));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/a2a-agents/{id}`.
async fn delete_a2a_agent(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query("DELETE FROM a2a_agents WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("a2a agent deletion", &e))?;
    if result.rows_affected() == 0 {
        return Err(api_error(StatusCode::NOT_FOUND, "no such agent"));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// An MCP server as the admin API accepts it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewMcpServer {
    name: String,
    url: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    auth_header: Option<String>,
    /// `""` sends the credential with no prefix, which is what several MCP
    /// hosts want. Absent means `Bearer`, exactly as for a model backend.
    #[serde(default)]
    auth_scheme: Option<String>,
    #[serde(default)]
    upstream_api_key: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// `GET /admin/mcp-servers`.
///
/// Reports **whether** a credential is set and never what it is — the same
/// rule `/admin/provider-models` follows, and the reason `upstream_api_key` is not in
/// the select list at all rather than filtered out afterwards.
async fn list_mcp_servers(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<serde_json::Value>, ApiError> {
    let rows: Vec<McpServerRow> = sqlx::query_as(
        "SELECT id, name, url, transport, description, auth_header, auth_scheme, enabled,
                    upstream_api_key IS NOT NULL
               FROM mcp_servers ORDER BY name",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("mcp server list", &e))?;

    let data: Vec<serde_json::Value> = rows
        .into_iter()
        .map(
            |(id, name, url, transport, description, auth_header, auth_scheme, enabled, cred)| {
                serde_json::json!({
                    "id": id,
                    "name": name,
                    "url": url,
                    "transport": transport,
                    "description": description,
                    "auth_header": auth_header,
                    "auth_scheme": auth_scheme,
                    "enabled": enabled,
                    "credential_set": cred,
                })
            },
        )
        .collect();
    Ok(Json(serde_json::json!({ "data": data })))
}

/// `POST /admin/mcp-servers`.
async fn post_mcp_server(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewMcpServer>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Checked here as well as by the column constraint, so the operator gets a
    // sentence instead of a Postgres error: the name is a path segment and a
    // tool-name namespace, and both constrain it.
    if !body
        .name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || body.name.is_empty()
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must be alphanumeric with - or _: it is both a URL path segment and the \
             namespace this server's tools appear under",
        ));
    }
    let transport = body.transport.unwrap_or_else(|| "http".to_string());
    if transport != "http" && transport != "sse" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "transport must be \"http\" (streamable) or \"sse\"",
        ));
    }
    let encrypted = body
        .upstream_api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| crate::control::secrets::encrypt(&ctx.key, k))
        .transpose()
        .map_err(|e| {
            tracing::error!(error = %e, "encrypting an MCP server credential failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not encrypt upstream_api_key; the server was not created",
            )
        })?;

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO mcp_servers
           (name, url, transport, description, auth_header, auth_scheme, upstream_api_key, enabled)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(&body.name)
    .bind(&body.url)
    .bind(&transport)
    .bind(&body.description)
    .bind(body.auth_header.as_deref().unwrap_or("authorization"))
    // Absent is `Bearer`; an explicit empty string is NULL, meaning send the
    // credential raw. Same three states as a model backend.
    .bind(match body.auth_scheme.as_deref() {
        None => Some("Bearer"),
        Some("") => None,
        Some(scheme) => Some(scheme),
    })
    .bind(encrypted)
    .bind(body.enabled.unwrap_or(true))
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!("an MCP server named {:?} already exists", body.name),
            )
        } else {
            db_error("mcp server creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": body.name })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchMcpServer {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    /// Absent leaves the stored credential alone; `""` clears it. Without the
    /// distinction there would be no way to edit a description without
    /// re-sending a secret.
    #[serde(default)]
    upstream_api_key: Option<String>,
}

/// `PATCH /admin/mcp-servers/{id}`.
async fn patch_mcp_server(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
    Json(body): Json<PatchMcpServer>,
) -> Result<StatusCode, ApiError> {
    let encrypted = match body.upstream_api_key.as_deref().map(str::trim) {
        Some("") => Some(None),
        Some(k) => Some(Some(
            crate::control::secrets::encrypt(&ctx.key, k).map_err(|e| {
                tracing::error!(error = %e, "encrypting an MCP server credential failed");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "could not encrypt upstream_api_key; nothing was changed",
                )
            })?,
        )),
        None => None,
    };
    let result = sqlx::query(
        "UPDATE mcp_servers
            SET url = COALESCE($2, url),
                description = COALESCE($3, description),
                enabled = COALESCE($4, enabled),
                upstream_api_key = CASE WHEN $5 THEN $6 ELSE upstream_api_key END,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.url.as_deref())
    .bind(body.description.as_deref())
    .bind(body.enabled)
    .bind(encrypted.is_some())
    .bind(encrypted.flatten())
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("mcp server update", &e))?;
    if result.rows_affected() == 0 {
        return Err(api_error(StatusCode::NOT_FOUND, "no such MCP server"));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/mcp-servers/{id}`.
///
/// Grants naming this server are left behind deliberately: they are rows in
/// `permissions`, an operator may be replacing the server, and a grant on a
/// name that does not exist simply matches nothing (`flatten_mcp_grants` only
/// keeps names that resolve).
async fn delete_mcp_server(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("mcp server deletion", &e))?;
    if result.rows_affected() == 0 {
        return Err(api_error(StatusCode::NOT_FOUND, "no such MCP server"));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
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
    // A provider model and a frontend model may share a name, and normally do.
    //
    // This used to be a 409, on the grounds that a client request naming it
    // would be ambiguous. It is not: `resolve_target_models` looks in
    // `frontend_models` first and falls through to a provider model only when
    // there is no frontend model of that name, so the frontend model wins,
    // deterministically. Migration 0034 relies on exactly that — it gives every
    // provider model a frontend model of the *same* name so the model stays
    // callable once frontend models are the only addressable surface, and
    // renaming the provider model out of the way instead would revoke every
    // grant naming it (migration 0029 did that in production).
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO provider_models (name, description, input_price_per_mtok, output_price_per_mtok, \
             cache_ttl_seconds, context_length) \
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(&body.name)
    .bind(&body.description)
    .bind(body.input_price_per_mtok)
    .bind(body.output_price_per_mtok)
    .bind(body.cache_ttl_seconds)
    .bind(body.context_length)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "a model named {:?} already exists; add another backend to it with \
                             POST /admin/provider-models/{{id}}/backends instead of creating a second model",
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

/// Fields a model can be changed to. Every one optional, and absent means
/// "leave it alone" rather than "clear it" — a `PATCH` that only sets a price
/// must not silently turn caching off.
///
/// Clearing is spelled explicitly: `null` sets the column to NULL, which is how
/// a model becomes unpriced or stops caching again.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchModel {
    #[serde(default, deserialize_with = "double_option")]
    description: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    input_price_per_mtok: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    output_price_per_mtok: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    cache_ttl_seconds: Option<Option<i32>>,
    /// Tokens this model accepts. `null` clears it back to undeclared, which
    /// is not the same as zero — see `ModelDef::context_length`.
    #[serde(default, deserialize_with = "double_option")]
    context_length: Option<Option<i64>>,
}

/// Tells "absent" apart from "present and null".
///
/// The distinction is the whole point of this handler: prices change, and
/// correcting one must not require re-sending every other field — nor should
/// omitting a field be indistinguishable from clearing it.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `PATCH /admin/provider-models/{id}`: change a model's description, prices or cache
/// TTL in place.
///
/// Exists because prices change. Without it the only way to correct one was to
/// delete the model — cascading its backends and their encrypted credentials —
/// and recreate the lot.
async fn patch_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
    Json(body): Json<PatchModel>,
) -> Result<StatusCode, ApiError> {
    for (name, value) in [
        ("input_price_per_mtok", body.input_price_per_mtok.flatten()),
        (
            "output_price_per_mtok",
            body.output_price_per_mtok.flatten(),
        ),
    ] {
        if value.is_some_and(|v| v < 0) {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("{name} cannot be negative"),
            ));
        }
    }
    if body.cache_ttl_seconds.flatten().is_some_and(|v| v < 0) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "cache_ttl_seconds cannot be negative; 0 or null turns caching off",
        ));
    }
    // Refused rather than coerced to "undeclared": a model that accepts no
    // tokens is not a thing, and silently reinterpreting the number would
    // leave an operator believing they had set a limit that is not there.
    if body.context_length.flatten().is_some_and(|v| v <= 0) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "context_length must be positive; send null to clear it",
        ));
    }

    // `COALESCE($n, column)` would make "set to null" impossible, so each field
    // carries its own "was it present" flag instead.
    let done = sqlx::query(
        "UPDATE provider_models SET
           description           = CASE WHEN $2 THEN $3  ELSE description           END,
           input_price_per_mtok  = CASE WHEN $4 THEN $5  ELSE input_price_per_mtok  END,
           output_price_per_mtok = CASE WHEN $6 THEN $7  ELSE output_price_per_mtok END,
           cache_ttl_seconds     = CASE WHEN $8 THEN $9  ELSE cache_ttl_seconds     END,
           context_length        = CASE WHEN $10 THEN $11 ELSE context_length        END
         WHERE id = $1",
    )
    .bind(id)
    .bind(body.description.is_some())
    .bind(body.description.clone().flatten().unwrap_or_default())
    .bind(body.input_price_per_mtok.is_some())
    .bind(body.input_price_per_mtok.flatten())
    .bind(body.output_price_per_mtok.is_some())
    .bind(body.output_price_per_mtok.flatten())
    .bind(body.cache_ttl_seconds.is_some())
    .bind(body.cache_ttl_seconds.flatten())
    .bind(body.context_length.is_some())
    .bind(body.context_length.flatten())
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("model update", &e))?;

    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no model with id {id}; GET /admin/provider-models lists the ids that exist"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let done = sqlx::query("DELETE FROM provider_models WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("model deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no model with id {id}; GET /admin/provider-models lists the ids that exist"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBackend {
    api_base: String,
    /// Absent means "the model's own name", matching what `File` mode does
    /// with a `litellm_params` entry that names no `model`.
    upstream_model: Option<String>,
    /// Encrypted before it reaches Postgres and never readable back through
    /// this API — `GET /admin/provider-models` reports only whether one is set.
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
/// Whether an endpoint looks like it is on our own network.
///
/// This only picks the initial `kind` label for a newly created provider, so
/// being wrong is a cosmetic matter an operator can correct — nothing routes,
/// authenticates or expires on it. It exists because "static" and "cloud"
/// differ in how they are *configured* (a typed address versus a catalogue
/// entry), and guessing right the overwhelming majority of the time is better
/// than making every operator answer a question they did not ask.
fn is_private_host(host: &str) -> bool {
    let h = host.split(':').next().unwrap_or(host);
    h == "localhost"
        || h.starts_with("127.")
        || h.starts_with("10.")
        || h.starts_with("192.168.")
        || h.strip_prefix("172.")
            .and_then(|rest| rest.split('.').next())
            .and_then(|octet| octet.parse::<u8>().ok())
            .is_some_and(|octet| (16..=31).contains(&octet))
}

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
    Path(provider_model_id): Path<i64>,
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
    // Fetched rather than assumed so a bad `provider_model_id` is reported as such,
    // and so `upstream_model` can default to the model's own name.
    let model_name: Option<String> =
        sqlx::query_scalar("SELECT name FROM provider_models WHERE id = $1")
            .bind(provider_model_id)
            .fetch_optional(&ctx.pool)
            .await
            .map_err(|e| db_error("model lookup", &e))?;
    let Some(model_name) = model_name else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "no model with id {provider_model_id}; GET /admin/provider-models lists the ids that exist"
            ),
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

    // A provider model has exactly one provider, so attaching a second is a
    // conflict rather than an addition. Refusing is the honest answer: the
    // caller wanted two upstreams for one name, and that is now a frontend
    // model with two targets, not a model with two backends.
    let existing: Option<i64> =
        sqlx::query_scalar("SELECT provider_id FROM provider_models WHERE id = $1")
            .bind(provider_model_id)
            .fetch_one(&ctx.pool)
            .await
            .map_err(|e| db_error("provider lookup", &e))?;
    if existing.is_some() {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "model {model_name:?} already has a provider; a provider model has exactly one. \
                 Detach it first, or create a frontend model with both as targets to balance \
                 across them"
            ),
        ));
    }

    // Same endpoint and auth means the same provider, which is the whole point
    // of the split: one credential shared by every model on it, so rotating it
    // is one write. A credential supplied here updates the provider's, since
    // the caller has just demonstrated a newer one.
    let provider_id: i64 = match sqlx::query_scalar::<_, i64>(
        "SELECT id FROM providers WHERE api_base = $1 AND protocol = $2 \
         AND auth_header = $3 AND auth_scheme IS NOT DISTINCT FROM $4",
    )
    .bind(&api_base)
    .bind(&protocol)
    .bind(&auth_header)
    .bind(&auth_scheme)
    .fetch_optional(&ctx.pool)
    .await
    .map_err(|e| db_error("provider lookup", &e))?
    {
        Some(id) => {
            if encrypted.is_some() {
                sqlx::query(
                    "UPDATE providers SET upstream_api_key = $1, credential_kind = $2 WHERE id = $3",
                )
                .bind(&encrypted)
                .bind(&credential_kind)
                .bind(id)
                .execute(&ctx.pool)
                .await
                .map_err(|e| db_error("provider credential update", &e))?;
            }
            id
        }
        None => {
            // host:port is what the Providers screen already called this
            // thing, so an operator sees the name they were reading before it
            // was a record. The suffix only appears when that is ambiguous.
            let host = api_base
                .split_once("://")
                .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
                .unwrap_or(&api_base)
                .to_string();
            let kind = if is_private_host(&host) {
                "static"
            } else {
                "cloud"
            };
            let mut name = host.clone();
            for n in 2..100 {
                let taken: bool =
                    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM providers WHERE name = $1)")
                        .bind(&name)
                        .fetch_one(&ctx.pool)
                        .await
                        .map_err(|e| db_error("provider name check", &e))?;
                if !taken {
                    break;
                }
                name = format!("{host}#{n}");
            }
            sqlx::query_scalar(
                "INSERT INTO providers (name, kind, api_base, protocol, auth_header, \
                 auth_scheme, upstream_api_key, credential_kind) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
            )
            .bind(&name)
            .bind(kind)
            .bind(&api_base)
            .bind(&protocol)
            .bind(&auth_header)
            .bind(&auth_scheme)
            .bind(&encrypted)
            .bind(&credential_kind)
            .fetch_one(&ctx.pool)
            .await
            .map_err(|e| db_error("provider creation", &e))?
        }
    };

    sqlx::query(
        "UPDATE provider_models SET provider_id = $1, upstream_model = $2, default_max_tokens = $3 \
         WHERE id = $4",
    )
    .bind(provider_id)
    .bind(&upstream_model)
    .bind(body.default_max_tokens)
    .bind(provider_model_id)
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("attaching provider", &e))?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": provider_model_id,
            "provider_id": provider_id,
            "provider_model_id": provider_model_id,
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
    // The id is the model's, because since migration 0029 the model *is* the
    // link to its provider. Detaching leaves the model, its price, its usage
    // history and any frontend model pointing at it untouched — it simply
    // stops being routable, which is the reversible half of "remove this
    // backend" and the only half this route should ever do.
    let done = sqlx::query(
        "UPDATE provider_models SET provider_id = NULL, upstream_model = NULL, \
         default_max_tokens = NULL WHERE id = $1 AND provider_id IS NOT NULL",
    )
    .bind(id)
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("detaching provider", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!(
                "no model with id {id} that has a provider; GET /admin/provider-models lists each \
                 model's provider"
            ),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

// --- Frontend models, routing rules and their targets ------------------
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
    provider_model_id: i64,
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
struct FrontendModelView {
    id: i64,
    name: String,
    description: String,
    rules: Vec<RuleView>,
    /// Used when no rule matches; see the migration's comment on
    /// `frontend_model_defaults` for why this is its own table rather than an
    /// always-true rule.
    default_targets: Vec<TargetView>,
    /// How targets are chosen between. Absent is the weighted split.
    policy: Option<String>,
}

// (id, owner_id, provider_model_id, model_name, weight, position) — `owner_id` is
// `rule_id` for a rule's own targets and `frontend_model_id` for a virtual
// model's defaults; the two queries below share this shape so `to_targets`
// works for either without duplicating it.
type TargetRow = (i64, i64, i64, String, i32, i32);

async fn list_virtual_models(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<FrontendModelView>>, ApiError> {
    let vms: Vec<(i64, String, String, Option<String>)> =
        sqlx::query_as("SELECT id, name, description, policy FROM frontend_models ORDER BY name")
            .fetch_all(&ctx.pool)
            .await
            .map_err(|e| db_error("listing frontend models", &e))?;
    let rules: Vec<(i64, i64, i32, serde_json::Value)> = sqlx::query_as(
        "SELECT id, frontend_model_id, position, match_json FROM routing_rules
         ORDER BY frontend_model_id, position",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing routing rules", &e))?;
    let rule_targets: Vec<TargetRow> = sqlx::query_as(
        "SELECT rt.id, rt.rule_id, rt.provider_model_id, m.name, rt.weight, rt.position
         FROM rule_targets rt JOIN provider_models m ON m.id = rt.provider_model_id
         ORDER BY rt.rule_id, rt.position",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing rule targets", &e))?;
    let default_targets: Vec<TargetRow> = sqlx::query_as(
        "SELECT vd.id, vd.frontend_model_id, vd.provider_model_id, m.name, vd.weight, vd.position
         FROM frontend_model_defaults vd JOIN provider_models m ON m.id = vd.provider_model_id
         ORDER BY vd.frontend_model_id, vd.position",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing frontend model defaults", &e))?;

    let to_targets = |owner: i64, rows: &[TargetRow]| -> Vec<TargetView> {
        rows.iter()
            .filter(|(_, o, ..)| *o == owner)
            .map(
                |(id, _, provider_model_id, model, weight, position)| TargetView {
                    id: *id,
                    provider_model_id: *provider_model_id,
                    model: model.clone(),
                    weight: *weight,
                    position: *position,
                },
            )
            .collect()
    };

    Ok(Json(
        vms.into_iter()
            .map(|(vm_id, name, description, policy)| {
                let rule_views = rules
                    .iter()
                    .filter(|(_, frontend_model_id, ..)| *frontend_model_id == vm_id)
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
                FrontendModelView {
                    id: vm_id,
                    name,
                    description,
                    rules: rule_views,
                    default_targets: to_targets(vm_id, &default_targets),
                    policy,
                }
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewFrontendModel {
    name: String,
    #[serde(default)]
    description: String,
    /// How to choose between this frontend model's targets:
    /// `cache-affinity`, `least-loaded`, `round-robin` or `lowest-latency`.
    /// Unset is the weighted split, which is what a target list has always
    /// meant and stays the default.
    ///
    /// This lives here rather than on a provider model because a provider
    /// model has one provider and therefore one backend — there is nothing
    /// for a policy to choose between. Targets are the things that need
    /// choosing between (migration 0038).
    #[serde(default)]
    policy: Option<String>,
}

async fn post_frontend_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewFrontendModel>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if body.name.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "name must not be empty; it is the name clients address this frontend model by",
        ));
    }
    // The same the other way round: putting a frontend model in front of a
    // provider model of the same name is the normal arrangement, not a clash.
    // See the note on the model-creation route.
    // Refused here rather than accepted and quietly ignored by every proxy
    // that reads it — the proxy falls back to the weighted split on a policy
    // it does not know, which is right for forward compatibility and wrong as
    // a response to a typo.
    let policy = validated_policy(body.policy.as_deref())?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO frontend_models (name, description, policy) VALUES ($1, $2, $3) \
         RETURNING id",
    )
    .bind(&body.name)
    .bind(&body.description)
    .bind(policy)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!("a frontend model named {:?} already exists", body.name),
            )
        } else {
            db_error("frontend model creation", &e)
        }
    })?;
    refresh(&ctx).await;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": body.name })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchFrontendModel {
    /// `null` clears it back to the weighted split; omitting it leaves the
    /// policy alone.
    #[serde(default, deserialize_with = "double_option")]
    policy: Option<Option<String>>,
}

/// `PATCH /admin/frontend-models/{id}` — change how targets are chosen between.
///
/// The policy lives here rather than on a provider model because a provider
/// model has one provider and therefore one backend; the targets are what need
/// choosing between (migration 0038).
async fn patch_frontend_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
    Json(body): Json<PatchFrontendModel>,
) -> Result<StatusCode, ApiError> {
    let policy = match &body.policy {
        Some(inner) => validated_policy(inner.as_deref())?,
        None => None,
    };
    let done = sqlx::query(
        "UPDATE frontend_models \
            SET policy = CASE WHEN $2 THEN $3 ELSE policy END \
          WHERE id = $1",
    )
    .bind(id)
    .bind(body.policy.is_some())
    .bind(policy)
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("frontend model update", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no frontend model with id {id}"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_virtual_model(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // ON DELETE CASCADE (migrations/0008) takes this frontend model's rules
    // and defaults with it, same reasoning as a principal's keys/grants.
    let done = sqlx::query("DELETE FROM frontend_models WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("frontend model deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!(
                "no frontend model with id {id}; GET /admin/frontend-models lists the ids that exist"
            ),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRule {
    /// Where this rule sits in its frontend model's evaluation order —
    /// load-bearing, not cosmetic: the first matching rule wins.
    position: i32,
    #[serde(flatten)]
    match_condition: MatchConditionJson,
}

async fn post_rule(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(frontend_model_id): Path<i64>,
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
        "INSERT INTO routing_rules (frontend_model_id, position, match_json)
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(frontend_model_id)
    .bind(body.position)
    .bind(match_json)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "no frontend model with id {frontend_model_id}; GET /admin/frontend-models \
                     lists the ids that exist"
                ),
            )
        } else if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "frontend model {frontend_model_id} already has a rule at position {}",
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
            serde_json::json!({ "id": id, "frontend_model_id": frontend_model_id, "position": body.position }),
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
            format!("no rule with id {id}; GET /admin/frontend-models lists each rule's id"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTarget {
    provider_model_id: i64,
    #[serde(default = "default_target_weight")]
    weight: i32,
    /// Failover order within the chain — see
    /// `crate::routing::FrontendModelDef::resolve`'s doc comment.
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
        // The name is copied from the model at write time and is what the
        // target is really bound to — the id only records which row carries
        // that name today, and goes NULL if it is deleted.
        "INSERT INTO rule_targets (rule_id, provider_model_id, target_provider_name, \
                                   target_model_name, weight, position)
         SELECT $1, pm.id, p.name, pm.name, $3, $4
           FROM provider_models pm LEFT JOIN providers p ON p.id = pm.provider_id
          WHERE pm.id = $2
         RETURNING id",
    )
    .bind(rule_id)
    .bind(body.provider_model_id)
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
                    body.provider_model_id
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
            format!("no rule target with id {id}; GET /admin/frontend-models lists each one's id"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn post_default_target(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(frontend_model_id): Path<i64>,
    Json(body): Json<NewTarget>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id: i64 = sqlx::query_scalar(
        // Same as a rule's target: bound by name, with the id as a cache.
        "INSERT INTO frontend_model_defaults (frontend_model_id, provider_model_id, \
                                              target_provider_name, target_model_name, \
                                              weight, position)
         SELECT $1, pm.id, p.name, pm.name, $3, $4
           FROM provider_models pm LEFT JOIN providers p ON p.id = pm.provider_id
          WHERE pm.id = $2
         RETURNING id",
    )
    .bind(frontend_model_id)
    .bind(body.provider_model_id)
    .bind(body.weight)
    .bind(body.position)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            api_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "no frontend model with id {frontend_model_id} or no model with id {}; a \
                     default target needs both to exist",
                    body.provider_model_id
                ),
            )
        } else if is_unique_violation(&e) {
            api_error(
                StatusCode::CONFLICT,
                format!(
                    "frontend model {frontend_model_id} already has a default target at position {}",
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
    let done = sqlx::query("DELETE FROM frontend_model_defaults WHERE id = $1")
        .bind(id)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("default target deletion", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!(
                "no default target with id {id}; GET /admin/frontend-models lists each one's id"
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

/// `POST /admin/prices/sync`: fill in prices from the published catalogues.
///
/// The same work `fastllm-proxy sync-prices` does, reachable from a UI. The
/// control plane already makes outbound calls (Vertex tokens), so this breaks
/// no invariant — it is the *request path* that performs no I/O, not this
/// process.
async fn sync_prices(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<SyncPricesRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let client = crate::control::gcp::shared_client().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no HTTP client is available to reach the price catalogues",
        )
    })?;
    let report = crate::control::pricing::sync(
        &ctx.pool,
        &client,
        body.source.unwrap_or(crate::control::pricing::Source::Both),
        body.overwrite,
        body.dry_run,
    )
    .await
    .map_err(|e| {
        tracing::error!(error = %format!("{e:#}"), "price sync failed");
        api_error(
            StatusCode::BAD_GATEWAY,
            format!("could not sync prices: {e}"),
        )
    })?;
    if !body.dry_run && report.updated > 0 {
        refresh(&ctx).await;
    }
    Ok(Json(serde_json::json!({
        "updated": report.updated,
        "already_priced": report.skipped,
        "unmatched": report.unmatched,
        "dry_run": body.dry_run,
        // The point of `dry_run`: a caller has to be able to see *what* would
        // change before agreeing to it. A count alone would make the preview
        // useless and the confirmation a formality.
        "changes": report.changes.iter().map(|(name, price)| serde_json::json!({
            "model": name,
            "input_price_per_mtok": price.input_per_mtok,
            "output_price_per_mtok": price.output_per_mtok,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncPricesRequest {
    #[serde(default)]
    source: Option<crate::control::pricing::Source>,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DryRunRequest {
    /// The name a client would put in `model`. A frontend model is the
    /// interesting case; a concrete one resolves to itself.
    model: String,
    #[serde(default)]
    principal_id: Option<i64>,
    #[serde(default)]
    streaming: bool,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
    /// A prompt class, as the classifier would have decided it. Supplied
    /// rather than computed: the control plane has the centroids but not
    /// necessarily the model, and asking "what would a `coding` prompt do"
    /// is the question a rule author actually has.
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    class_refines: Vec<String>,
}

#[derive(Serialize)]
struct DryRunResult {
    /// The chain, best first. Empty means no rule matched and there were no
    /// defaults — the request would 404.
    candidates: Vec<String>,
    /// Which rule decided, by position, or `None` for the defaults.
    matched_rule: Option<usize>,
    /// `false` when the name is a provider model, which resolves to itself.
    frontend_model: bool,
}

/// `POST /admin/routing/dry-run`: what would this request route to?
///
/// Answers the question a rule author actually has — "does my `coding` rule
/// fire for this caller" — without sending a real request to a real model and
/// reading the answer out of a log.
///
/// Two honest limits. Backend **health** is not consulted: this registry is
/// built fresh from the snapshot, so every backend looks up. A dry-run tells
/// you which rule matches, not which replica is currently reachable — that is
/// what `GET /admin/fleet` is for. And the class is supplied rather than
/// computed, so this says what a `coding` prompt would do, not whether a
/// particular prompt is coding.
async fn routing_dry_run(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
    Json(body): Json<DryRunRequest>,
) -> Result<Json<DryRunResult>, ApiError> {
    let snapshot = ctx.cache.current_snapshot();
    let registry = crate::registry::Registry::build_from_snapshot(
        &snapshot,
        &crate::registry::Interner::default(),
        None,
    )
    .map_err(|e| {
        tracing::error!(error = %e, "dry run could not build a registry");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not evaluate routing; see server logs",
        )
    })?;

    let Some(vm) = snapshot.frontend_models.get(&body.model) else {
        // A provider model routes to itself, which is worth answering rather
        // than erroring: a UI should be able to ask about any name.
        return Ok(Json(DryRunResult {
            candidates: vec![body.model.clone()],
            matched_rule: None,
            frontend_model: false,
        }));
    };

    let principal = body
        .principal_id
        .and_then(|id| snapshot.principals.values().find(|p| p.id as i64 == id));
    let mut headers = HeaderMap::new();
    for (name, value) in &body.headers {
        if let (Ok(n), Ok(v)) = (
            name.parse::<axum::http::HeaderName>(),
            value.parse::<axum::http::HeaderValue>(),
        ) {
            headers.insert(n, v);
        }
    }
    let facts = crate::routing::RequestFacts {
        caller: principal,
        prompt_tokens: body.prompt_tokens,
        max_tokens: body.max_tokens,
        streaming: body.streaming,
        headers: &headers,
        now: chrono::Utc::now(),
        class: body.class.as_deref(),
        class_refines: &body.class_refines,
    };

    // The same prefix hash a real request would produce is unavailable without
    // its body, and weighted targets are chosen from it. Zero is deterministic
    // and documented rather than random, so a dry run is reproducible.
    let candidates = vm.resolve_candidates(&facts, 0, &registry);
    let matched_rule = vm.rules.iter().position(|r| r.matches(&facts, &registry));

    Ok(Json(DryRunResult {
        candidates,
        matched_rule,
        frontend_model: true,
    }))
}

/// `POST /health-report`: a proxy telling the control plane what it can see.
///
/// On the proxy token, alongside `/snapshot` and `/usage`, for the same reason
/// those are: this is a proxy process authenticating to the control plane, not
/// a human with a password.
async fn post_health_report(
    State(ctx): State<Ctx>,
    headers: HeaderMap,
    Json(report): Json<crate::health_report::HealthReport>,
) -> impl IntoResponse {
    if !proxy_token_authorised(&headers, &ctx.proxy_token) {
        return StatusCode::UNAUTHORIZED;
    }
    let previous = ctx.fleet.record(report.clone(), std::time::Instant::now());
    notify_health_transitions(&ctx, &report, previous.as_ref());
    persist_rejection_deltas(&ctx.pool, &report, previous.as_ref()).await;
    StatusCode::NO_CONTENT
}

/// Emit a notification for each backend that changed health since this
/// replica's last report.
///
/// Transitions, not states: a backend that is down stays down, and a report
/// every ten seconds would become ten alerts a minute for one incident. The
/// previous report is already in hand from `Fleet::record`, so the comparison
/// costs nothing extra and needs no state of its own.
///
/// Per replica rather than fleet-wide, deliberately. Every replica losing a
/// backend is a dead backend; one replica losing it is a partition. Merging
/// them here would delete the distinction before anyone saw it — the same
/// reasoning `GET /admin/fleet` follows by never averaging replicas together.
///
/// A replica's *first* report emits nothing. There is no previous state to
/// have changed from, and a control plane restart would otherwise announce
/// every already-down backend in the fleet as though it had just failed.
fn notify_health_transitions(
    ctx: &Ctx,
    report: &crate::health_report::HealthReport,
    previous: Option<&crate::health_report::HealthReport>,
) {
    let Some(prev) = previous else { return };
    for backend in &report.backends {
        let was = prev
            .backends
            .iter()
            .find(|b| b.api_base == backend.api_base && b.model == backend.model);
        let Some(was) = was else { continue };
        if was.healthy == backend.healthy {
            continue;
        }
        let event = if backend.healthy {
            crate::webhook::Event::BackendRecovered {
                replica: report.replica.clone(),
                api_base: backend.api_base.clone(),
                model: backend.model.clone(),
            }
        } else {
            crate::webhook::Event::BackendDown {
                replica: report.replica.clone(),
                api_base: backend.api_base.clone(),
                model: backend.model.clone(),
            }
        };
        ctx.webhook.send(event);
    }
}

/// Store the increase in this replica's unattributable-refusal counters
/// since its last report.
///
/// These are the refusals `usage_events` cannot hold — a 401 has no
/// principal to attribute and a 404 has no model — so without this an error
/// rate computed from the database is a count of failures that got far
/// enough to be attributed, which is not the number anyone means by "errors
/// callers saw".
///
/// Deltas rather than samples, because the subtraction needs the previous
/// value and this is the one place it is already in hand. A counter that
/// went *down* means the replica restarted with fresh counters, so the new
/// value is the delta — the alternative, a negative, would subtract real
/// failures from other replicas' totals in the same bucket.
///
/// The first report from a replica is skipped entirely rather than counted
/// from zero. Its counter covers however long that process has been alive,
/// which after a control-plane restart can be days, and attributing all of
/// it to the current minute would draw a spike that never happened.
///
/// Failure is logged and swallowed: this is diagnostic bookkeeping arriving
/// on a health report, and returning an error would make a replica retry a
/// report whose real purpose — publishing backend health — already
/// succeeded.
async fn persist_rejection_deltas(
    pool: &PgPool,
    report: &crate::health_report::HealthReport,
    previous: Option<&crate::health_report::HealthReport>,
) {
    let Some(prev) = previous else { return };
    let delta = |now: u64, before: u64| -> i64 {
        if now >= before {
            (now - before) as i64
        } else {
            now as i64
        }
    };
    let rows = [
        (
            "unauthenticated",
            delta(
                report.process.rejected_unauthenticated,
                prev.process.rejected_unauthenticated,
            ),
        ),
        (
            "model_not_found",
            delta(
                report.process.rejected_model_not_found,
                prev.process.rejected_model_not_found,
            ),
        ),
    ];
    for (kind, count) in rows {
        if count == 0 {
            continue;
        }
        // Bucketed to the minute and summed on conflict: reports arrive
        // every ten seconds, so several land in the same bucket, and each
        // carries a distinct slice of time that must add rather than
        // overwrite.
        if let Err(e) = sqlx::query(
            "INSERT INTO gateway_rejections (at, replica, kind, count)
             VALUES (date_trunc('minute', now()), $1, $2, $3)
             ON CONFLICT (at, replica, kind)
             DO UPDATE SET count = gateway_rejections.count + EXCLUDED.count",
        )
        .bind(&report.replica)
        .bind(kind)
        .bind(count)
        .execute(pool)
        .await
        {
            tracing::warn!(error = %e, kind, "could not record gateway rejections");
        }
    }
}

/// `GET /admin/fleet`: what every proxy currently reports.
///
/// Per replica and not merged, because the interesting failures are the ones
/// where replicas disagree — a proxy that cannot reach a backend the others can
/// is a partition, and a fleet-wide average hides the only symptom there is.
async fn list_fleet(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Json<Vec<crate::health_report::HealthReport>> {
    Json(ctx.fleet.current(std::time::Instant::now()))
}

/// Usage rolled up over a window.
#[derive(Serialize, Debug)]
struct UsageSummary {
    /// Whatever the caller grouped by: a principal name, a model name, or a
    /// day. `None` for a row whose key is null (usage attributed to a model
    /// that has since been deleted).
    key: Option<String>,
    requests: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    /// Summed over the rows that *have* a cost.
    cost_micros: i64,
    /// How many of `requests` contributed nothing to `cost_micros` because the
    /// model is unpriced and no provider reported a figure.
    ///
    /// Published rather than folded in, because `SUM` over a nullable column
    /// treats those rows as zero and the total then reads as "this was cheap"
    /// when it means "this is unknown". A caller showing a spend figure needs
    /// to know how much of the traffic it does not cover.
    unpriced_requests: i64,
}

/// One bucket of a time series. Every field is present for every bucket,
/// including empty ones — see [`timeseries`] for why gaps are not an option.
#[derive(Serialize)]
pub struct TimeseriesPoint {
    /// Start of the bucket, RFC 3339.
    pub at: chrono::DateTime<chrono::Utc>,
    /// Everything attributable that happened in this bucket: forwarded
    /// responses *and* refusals. The two breakdowns below partition it.
    pub requests: i64,
    /// Responses a backend returned with a 4xx or 5xx status.
    pub upstream_errors: i64,
    /// Requests the gateway turned away itself, by kind. Separate from
    /// `upstream_errors` on purpose: the remedies have nothing in common.
    pub refused_authorisation: i64,
    pub refused_rate_limit: i64,
    pub refused_budget: i64,
    pub refused_no_backend: i64,
    /// Refusals that never reached attribution — a 401 with no valid key, a
    /// 404 for a model that does not exist. Counted from
    /// `gateway_rejections` rather than `usage_events`, because there is no
    /// principal to key a row on; see that table's comment.
    ///
    /// Excluded when the caller filters by model or principal: an
    /// unauthenticated request has neither, so including it would make every
    /// filtered view report failures it cannot attribute to the thing being
    /// filtered for.
    pub refused_unattributed: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_micros: i64,
    /// Requests contributing nothing to `cost_micros`, so a spend line can
    /// say how much of the traffic it does not cover rather than implying
    /// the rest was free.
    pub unpriced_requests: i64,
    /// Whole-request latency percentiles, in milliseconds, over the
    /// forwarded responses in this bucket that were timed. `None` for a
    /// bucket with nothing to measure — which is not the same as zero, and
    /// a chart must break the line rather than plot it at the axis.
    p50_ms: Option<i64>,
    p95_ms: Option<i64>,
    /// Time to first token, streamed responses only.
    ttft_p95_ms: Option<i64>,
}

/// The raw shape one bucket comes back as. A named struct rather than a
/// fourteen-element tuple: the columns are positional in the SQL either way,
/// but a mismatch between query and destructuring is a compile error here and
/// a silently transposed column there.
#[derive(sqlx::FromRow)]
struct TimeseriesRow {
    at: chrono::DateTime<chrono::Utc>,
    requests: i64,
    upstream_errors: i64,
    r_auth: i64,
    r_rate: i64,
    r_budget: i64,
    r_nobackend: i64,
    r_unattributed: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    cost_micros: i64,
    unpriced: i64,
    p50: Option<f64>,
    p95: Option<f64>,
    ttft95: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimeseriesQuery {
    #[serde(default)]
    since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    until: Option<chrono::DateTime<chrono::Utc>>,
    /// Bucket width in seconds. Clamped, and snapped to a sane value by
    /// `bucket_seconds` rather than trusted.
    #[serde(default)]
    bucket: Option<i64>,
    /// Restrict to one model, by name.
    #[serde(default)]
    model: Option<String>,
    /// Restrict to one principal, by id.
    #[serde(default)]
    principal_id: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageQuery {
    #[serde(default = "default_group_by")]
    group_by: String,
    #[serde(default)]
    since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default = "default_usage_limit")]
    limit: i64,
}

fn default_group_by() -> String {
    "model".to_string()
}

/// Snap a requested bucket width to one that will not produce an absurd
/// number of points, and pick a sensible one when none was asked for.
///
/// The cap is on *points returned*, not on the width, because that is what
/// actually hurts: a one-second bucket over thirty days is 2.6 million rows
/// that no chart can draw and no browser should receive. Widening until the
/// series fits answers the question the caller meant — "show me this range"
/// — instead of refusing, and the response says which width was used so the
/// axis can be labelled honestly rather than guessed at.
fn bucket_seconds(requested: Option<i64>, span_seconds: i64) -> i64 {
    /// Enough for a dense chart, few enough to stay a small response.
    const MAX_POINTS: i64 = 720;
    const LADDER: [i64; 10] = [
        10,      // ten seconds
        30,      //
        60,      // a minute
        300,     // five
        900,     // fifteen
        3_600,   // an hour
        21_600,  // six
        86_400,  // a day
        604_800, // a week
        2_592_000,
    ];
    let span = span_seconds.max(1);
    let floor = (span + MAX_POINTS - 1) / MAX_POINTS;
    let wanted = requested.unwrap_or(0).max(floor).max(1);
    LADDER
        .iter()
        .copied()
        .find(|&step| step >= wanted)
        .unwrap_or(LADDER[LADDER.len() - 1])
}

/// `GET /admin/timeseries`: bucketed traffic, latency and spend over a
/// window, for the charts.
///
/// # Why this exists rather than the UI polling `/admin/usage` repeatedly
///
/// The old screens computed rates in the browser by diffing two polls of a
/// counter, which meant the numbers began at nothing on every page load and
/// vanished on reload — there was no history because none was ever asked
/// for. This reads the history that `usage_events` has been accumulating
/// since usage recording was widened to every request, so a chart can show
/// yesterday as readily as the last minute.
///
/// # Empty buckets are zeros, not gaps
///
/// The `generate_series` join is the whole point of the query's shape. An
/// aggregate alone returns rows only where events exist, and a chart drawn
/// from that connects 09:00 straight to 11:00 with a smooth line across an
/// hour when the gateway served nothing — turning an outage into an
/// interpolation. Emitting an explicit zero makes the hole visible as a
/// hole.
///
/// Latency is the exception, and deliberately: `p50_ms` is `None` for a
/// bucket with nothing to measure, because a zero there would read as
/// "instantaneous" rather than "no data". The two want opposite treatments
/// from a chart — a count of zero is a real point on the axis, an unknown
/// latency is a break in the line.
/// The query behind `GET /admin/timeseries`, separated from the handler so an
/// integration test can run it against a real database.
///
/// `sqlx::query_as` does not verify this statement at compile time. A
/// malformed one — a missing comma between two CTEs, say — compiles cleanly,
/// passes every unit test, and fails only when Postgres parses it. That
/// shipped once: the endpoint returned 500 and the only symptom on screen
/// was a chart showing its "control plane too old" fallback.
async fn timeseries_rows(
    pool: &PgPool,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
    bucket_seconds: i64,
    model: Option<String>,
    principal_id: Option<i64>,
) -> Result<Vec<TimeseriesPoint>, sqlx::Error> {
    let rows: Vec<TimeseriesRow> = sqlx::query_as(
        "WITH grid AS (
             SELECT generate_series(
                 to_timestamp(floor(extract(epoch FROM $1::timestamptz) / $3) * $3),
                 $2::timestamptz,
                 make_interval(secs => $3)
             ) AS at
         ),
         -- Raw rows and rolled-up buckets, unioned before aggregation.
         --
         -- Without this the charts would simply end at the retention
         -- boundary: a 90-day window would show nothing beyond it, and the
         -- roll-up would be a table nothing ever read. `refusal` and
         -- `status` are reconstructed from the rollup's counters so one
         -- aggregation expression works over both shapes.
         --
         -- `duration_ms` is NULL for every rolled-up row on purpose.
         -- Percentiles do not merge, so a rolled-up bucket has no p50 or p95
         -- to report and says so, rather than offering a mean wearing a
         -- percentile's name. The chart breaks its line there, the same way
         -- it does for a bucket with nothing in it.
         src AS (
             SELECT u.at, u.provider_model_id, u.principal_id, u.status, u.refusal,
                    u.prompt_tokens, u.completion_tokens, u.cost_micros,
                    u.duration_ms, u.ttft_ms, 1::bigint AS weight
             FROM usage_events u
             WHERE u.at >= $1 AND u.at < $2
             UNION ALL
             -- One rollup row carries several outcomes as parallel counters,
             -- so it expands back into one row per outcome, each weighted.
             -- Collapsing it into a single row would keep the totals and
             -- lose the breakdown -- errors and refusals would silently read
             -- zero for every bucket older than the retention window, which
             -- is precisely the shape of bug this whole feature exists to
             -- stop the charts from having.
             --
             -- Tokens and cost ride on the served row alone so the sums stay
             -- right; the failure rows carry counts only.
             SELECT r.hour, r.provider_model_id, r.principal_id, o.status, o.refusal,
                    CASE WHEN o.kind = 'ok' THEN r.prompt_tokens ELSE 0 END,
                    CASE WHEN o.kind = 'ok' THEN r.completion_tokens ELSE 0 END,
                    CASE WHEN o.kind = 'ok' THEN NULLIF(r.cost_micros, 0) END,
                    NULL::int, NULL::int, o.weight
             FROM usage_rollup_hourly r
             CROSS JOIN LATERAL (VALUES
                 ('ok',   200::smallint, NULL::text,
                  r.requests - r.upstream_errors - r.refused_authorisation
                             - r.refused_rate_limit - r.refused_budget - r.refused_no_backend),
                 ('err',  500::smallint, NULL,            r.upstream_errors),
                 ('ref',  403::smallint, 'authorisation', r.refused_authorisation),
                 ('ref',  429::smallint, 'rate_limit',    r.refused_rate_limit),
                 ('ref',  402::smallint, 'budget',        r.refused_budget),
                 ('ref',  502::smallint, 'no_backend',    r.refused_no_backend)
             ) AS o(kind, status, refusal, weight)
             WHERE r.hour >= $1 AND r.hour < $2 AND o.weight > 0
         ),
         ev AS (
             SELECT to_timestamp(floor(extract(epoch FROM u.at) / $3) * $3) AS at,
                    COALESCE(sum(u.weight), 0)::bigint            AS requests,
                    COALESCE(sum(u.weight) FILTER (
                        WHERE u.refusal IS NULL AND u.status >= 400), 0)::bigint AS upstream_errors,
                    COALESCE(sum(u.weight) FILTER (WHERE u.refusal = 'authorisation'), 0)::bigint AS r_auth,
                    COALESCE(sum(u.weight) FILTER (WHERE u.refusal = 'rate_limit'), 0)::bigint AS r_rate,
                    COALESCE(sum(u.weight) FILTER (WHERE u.refusal = 'budget'), 0)::bigint AS r_budget,
                    COALESCE(sum(u.weight) FILTER (WHERE u.refusal = 'no_backend'), 0)::bigint AS r_nobackend,
                    COALESCE(sum(u.prompt_tokens), 0)::bigint      AS prompt_tokens,
                    COALESCE(sum(u.completion_tokens), 0)::bigint  AS completion_tokens,
                    COALESCE(sum(u.cost_micros), 0)::bigint        AS cost_micros,
                    COALESCE(sum(u.weight) FILTER (WHERE u.cost_micros IS NULL), 0)::bigint AS unpriced,
                    percentile_cont(0.5) WITHIN GROUP (ORDER BY u.duration_ms)  AS p50,
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY u.duration_ms) AS p95,
                    percentile_cont(0.95) WITHIN GROUP (ORDER BY u.ttft_ms)     AS ttft95
             FROM src u
             LEFT JOIN provider_models m ON m.id = u.provider_model_id
             WHERE ($4::text IS NULL OR m.name = $4)
               AND ($5::bigint IS NULL OR u.principal_id = $5)
             GROUP BY 1
         ),
         rej AS (
             SELECT to_timestamp(floor(extract(epoch FROM g.at) / $3) * $3) AS at,
                    COALESCE(sum(g.count), 0)::bigint AS unattributed
             FROM gateway_rejections g
             WHERE g.at >= $1 AND g.at < $2
               -- Only when nothing is being filtered for. These rows carry
               -- no model and no principal, so a filtered view that included
               -- them would attribute anonymous failures to whichever thing
               -- the operator happened to be looking at.
               AND $4::text IS NULL AND $5::bigint IS NULL
             GROUP BY 1
         )
         SELECT grid.at                                AS at,
                COALESCE(ev.requests, 0)               AS requests,
                COALESCE(ev.upstream_errors, 0)        AS upstream_errors,
                COALESCE(ev.r_auth, 0)                 AS r_auth,
                COALESCE(ev.r_rate, 0)                 AS r_rate,
                COALESCE(ev.r_budget, 0)               AS r_budget,
                COALESCE(ev.r_nobackend, 0)            AS r_nobackend,
                COALESCE(rej.unattributed, 0)          AS r_unattributed,
                COALESCE(ev.prompt_tokens, 0)          AS prompt_tokens,
                COALESCE(ev.completion_tokens, 0)      AS completion_tokens,
                COALESCE(ev.cost_micros, 0)            AS cost_micros,
                COALESCE(ev.unpriced, 0)               AS unpriced,
                ev.p50                                 AS p50,
                ev.p95                                 AS p95,
                ev.ttft95                              AS ttft95
         FROM grid
         LEFT JOIN ev  ON ev.at  = grid.at
         LEFT JOIN rej ON rej.at = grid.at
         ORDER BY grid.at",
    )
    .bind(since)
    .bind(until)
    .bind(bucket_seconds as f64)
    .bind(&model)
    .bind(principal_id)
    .fetch_all(pool)
    .await
    ?;

    Ok(rows
        .into_iter()
        .map(|r| TimeseriesPoint {
            at: r.at,
            requests: r.requests,
            upstream_errors: r.upstream_errors,
            refused_authorisation: r.r_auth,
            refused_rate_limit: r.r_rate,
            refused_budget: r.r_budget,
            refused_no_backend: r.r_nobackend,
            refused_unattributed: r.r_unattributed,
            prompt_tokens: r.prompt_tokens,
            completion_tokens: r.completion_tokens,
            cost_micros: r.cost_micros,
            unpriced_requests: r.unpriced,
            // Rounded here rather than in SQL: `percentile_cont`
            // interpolates and returns a double, and a millisecond
            // figure with fifteen decimal places is noise in a tooltip.
            p50_ms: r.p50.map(|v| v.round() as i64),
            p95_ms: r.p95.map(|v| v.round() as i64),
            ttft_p95_ms: r.ttft95.map(|v| v.round() as i64),
        })
        .collect())
}

/// The timeseries query, reachable from the integration test that proves it
/// parses.
///
/// `sqlx::query_as` does not check this statement at compile time, so a
/// malformed one compiles, passes every unit test, and fails when Postgres
/// reads it. That is not hypothetical — a missing comma between two CTEs
/// shipped, and the only symptom was a chart showing its "control plane too
/// old" fallback while the endpoint returned 500.
pub async fn timeseries_for_test(
    pool: &PgPool,
    bucket_seconds: i64,
    model: Option<&str>,
    principal_id: Option<i64>,
) -> Result<Vec<TimeseriesPoint>, sqlx::Error> {
    let until = chrono::Utc::now();
    let since = until - chrono::Duration::hours(24);
    timeseries_rows(
        pool,
        since,
        until,
        bucket_seconds,
        model.map(str::to_owned),
        principal_id,
    )
    .await
}

async fn timeseries(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
    axum::extract::Query(q): axum::extract::Query<TimeseriesQuery>,
) -> Result<Json<Vec<TimeseriesPoint>>, ApiError> {
    let until = q.until.unwrap_or_else(chrono::Utc::now);
    let since = q
        .since
        .unwrap_or_else(|| until - chrono::Duration::hours(24));
    if since >= until {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "since must be before until",
        ));
    }
    let bucket = bucket_seconds(q.bucket, (until - since).num_seconds());

    // `to_timestamp(floor(extract(epoch ...) / n) * n)` rather than
    // `date_trunc`: `date_trunc` only knows named units, and the ladder above
    // includes widths like five and fifteen minutes that have no name. This
    // form buckets to an arbitrary number of seconds, and both sides of the
    // join use it so the generated grid and the aggregated rows land on
    // identical instants -- an off-by-one there shows up as every bucket
    // reading zero, which is a confusing way to find out.
    let rows = timeseries_rows(
        &ctx.pool,
        since,
        until,
        bucket,
        q.model.clone(),
        q.principal_id,
    )
    .await
    .map_err(|e| db_error("timeseries", &e))?;
    Ok(Json(rows))
}

fn default_usage_limit() -> i64 {
    100
}

/// `GET /admin/usage`: aggregate usage and spend.
///
/// Grouped rather than paginated over raw rows: a screen wants "spend by team
/// this month", and reading a million events to compute it client-side is the
/// thing this endpoint exists to prevent.
async fn usage_summary(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
    axum::extract::Query(q): axum::extract::Query<UsageQuery>,
) -> Result<Json<Vec<UsageSummary>>, ApiError> {
    // An allow-list, not interpolation: this is the one place a caller's
    // string would reach the query text.
    let key_expr = match q.group_by.as_str() {
        // The name recorded on the event, not the one joined from `models`.
        // A model that has since been deleted has no row to join to, and
        // every such request would otherwise collapse into one nameless
        // bucket — which is the same disappearance the cascade used to cause,
        // moved from the data to the report. `m.name` is the fallback for
        // rows written before `model_name` existed.
        "model" => "coalesce(u.model_name, m.name)",
        "principal" => "p.name",
        "day" => "to_char(date_trunc('day', u.at), 'YYYY-MM-DD')",
        // What the *caller asked for*, which for a frontend model is the
        // virtual name and for anything else is the model itself. Grouping on
        // this answers "how much traffic does each frontend model carry",
        // which grouping on the served model cannot: by then the routing
        // decision has already been made and the virtual name is gone.
        "frontend_model" => "coalesce(u.requested_model, u.model_name, m.name)",
        other => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("group_by {other:?} is not one of: model, principal, day, frontend_model"),
            ))
        }
    };
    let limit = q.limit.clamp(1, 1000);

    let sql = format!(
        "SELECT {key_expr} AS key,
                count(*)                                        AS requests,
                -- `sum()` over a bigint returns *numeric* in Postgres, not
                -- bigint, so each of these needs the cast back or the decode
                -- fails at runtime with nothing wrong in the SQL itself.
                COALESCE(sum(u.prompt_tokens), 0)::bigint       AS prompt_tokens,
                COALESCE(sum(u.completion_tokens), 0)::bigint   AS completion_tokens,
                COALESCE(sum(u.cost_micros), 0)::bigint         AS cost_micros,
                count(*) FILTER (WHERE u.cost_micros IS NULL) AS unpriced_requests
         FROM usage_events u
         JOIN principals p ON p.id = u.principal_id
         LEFT JOIN provider_models m ON m.id = u.provider_model_id
         WHERE ($1::timestamptz IS NULL OR u.at >= $1)
           AND ($2::timestamptz IS NULL OR u.at <  $2)
         GROUP BY 1
         ORDER BY cost_micros DESC, requests DESC
         LIMIT $3"
    );
    let rows: Vec<(Option<String>, i64, i64, i64, i64, i64)> = sqlx::query_as(&sql)
        .bind(q.since)
        .bind(q.until)
        .bind(limit)
        .fetch_all(&ctx.pool)
        .await
        .map_err(|e| db_error("summarising usage", &e))?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(key, requests, prompt_tokens, completion_tokens, cost_micros, unpriced)| {
                    UsageSummary {
                        key,
                        requests,
                        prompt_tokens,
                        completion_tokens,
                        cost_micros,
                        unpriced_requests: unpriced,
                    }
                },
            )
            .collect(),
    ))
}

/// The verbs a role may be granted, and the only ones this API will write.
///
/// A closed list because a permission is only meaningful if something checks
/// it: inventing `model:delete` here would produce a row that grants nothing
/// and reads, on a permission matrix, as though it did.
const GRANTABLE_VERBS: &[&str] = &[
    admin_permission::READ,
    admin_permission::KEY_CREATE,
    admin_permission::KEY_REVOKE,
    admin_permission::CONFIG_WRITE,
    "model:invoke",
    "mcp:invoke",
    "agent:invoke",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantPermission {
    verb: String,
    /// `*` for the admin verbs, or `model/<name>` for `model:invoke`. Absent
    /// means `*`, which is what every admin verb wants and what a caller
    /// toggling a matrix cell would otherwise have to know.
    #[serde(default)]
    resource: Option<String>,
}

/// `POST /admin/roles/{name}/permissions`: grant a verb to a role.
///
/// Idempotent — granting twice is not an error, because a permission matrix
/// toggled twice should end up in the state the operator sees, not a 409.
async fn grant_permission(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(role): Path<String>,
    Json(body): Json<GrantPermission>,
) -> Result<StatusCode, ApiError> {
    let resource = body.resource.unwrap_or_else(|| "*".into());
    validate_grant(&body.verb, &resource)?;

    let role_id: Option<i64> = sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
        .bind(&role)
        .fetch_optional(&ctx.pool)
        .await
        .map_err(|e| db_error("looking up the role", &e))?;
    let Some(role_id) = role_id else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no role named {role:?}; GET /admin/roles lists them"),
        ));
    };

    // The permission row is created on demand: `model:invoke` on a specific
    // model is one row per model, and an operator granting access to a new
    // model should not have to create the permission first.
    let permission_id: i64 = sqlx::query_scalar(
        "INSERT INTO permissions (verb, resource) VALUES ($1, $2)
         ON CONFLICT (verb, resource) DO UPDATE SET verb = EXCLUDED.verb
         RETURNING id",
    )
    .bind(&body.verb)
    .bind(&resource)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| db_error("creating the permission", &e))?;

    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(role_id)
    .bind(permission_id)
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("granting the permission", &e))?;

    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /admin/roles/{name}/permissions`: revoke one.
///
/// The permission row itself is left alone — other roles may hold it, and a
/// row with no holders is inert.
async fn revoke_permission(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(role): Path<String>,
    Json(body): Json<GrantPermission>,
) -> Result<StatusCode, ApiError> {
    let resource = body.resource.unwrap_or_else(|| "*".into());
    let done = sqlx::query(
        "DELETE FROM role_permissions rp
         USING roles r, permissions p
         WHERE rp.role_id = r.id AND rp.permission_id = p.id
           AND r.name = $1 AND p.verb = $2 AND p.resource = $3",
    )
    .bind(&role)
    .bind(&body.verb)
    .bind(&resource)
    .execute(&ctx.pool)
    .await
    .map_err(|e| db_error("revoking the permission", &e))?;

    // Revoking something already absent is the state the caller asked for.
    if done.rows_affected() > 0 {
        refresh(&ctx).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Reject a grant that would read as meaningful and enforce nothing.
fn validate_grant(verb: &str, resource: &str) -> Result<(), ApiError> {
    if !GRANTABLE_VERBS.contains(&verb) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "verb {verb:?} is not one of: {}",
                GRANTABLE_VERBS.join(", ")
            ),
        ));
    }
    // Both scoped verbs, and both for the same reason: a resource that does
    // not match the expected prefix silently matches nothing, which on a
    // permission matrix reads as access granted.
    if let Some(prefix) = match verb {
        "model:invoke" => Some("model/"),
        "mcp:invoke" => Some("mcp/"),
        "agent:invoke" => Some("agent/"),
        _ => None,
    } {
        if !resource.starts_with(prefix) {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                format!("{verb} needs a resource of {prefix}* or {prefix}<name>"),
            ));
        }
    } else if resource != "*" {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("verb {verb:?} is only meaningful with resource *"),
        ));
    }
    Ok(())
}

/// One recorded configuration change.
#[derive(Serialize, Debug)]
struct AuditView {
    id: i64,
    /// The name as it was when the change was made. Kept even after the
    /// principal is deleted — a trail that disappears with the account that
    /// did the thing is not a trail.
    actor_name: String,
    actor_id: Option<i64>,
    action: String,
    target: String,
    detail: serde_json::Value,
    at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    /// Newest first, so a UI's first page is the interesting one.
    #[serde(default = "default_audit_limit")]
    limit: i64,
    /// Keyset rather than an offset: `before` is the id of the oldest row the
    /// caller already has. An offset would skip or repeat rows as new ones
    /// arrive at the head, which on an append-only log is guaranteed.
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    actor_id: Option<i64>,
    /// Substring, so `/admin/keys` finds every route under it.
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    since: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_audit_limit() -> i64 {
    100
}

/// `GET /admin/audit`: read the configuration-change log.
///
/// Written by a layer over every `/admin/*` mutation; this is how it is read
/// back without reaching for `psql`.
async fn list_audit(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
    axum::extract::Query(q): axum::extract::Query<AuditQuery>,
) -> Result<Json<Vec<AuditView>>, ApiError> {
    // Clamped rather than rejected: a UI asking for more than this wants
    // "lots", and an unbounded page is how one screen reads a year of history
    // into memory.
    let limit = q.limit.clamp(1, 1000);
    type AuditRow = (
        i64,
        Option<i64>,
        String,
        String,
        String,
        serde_json::Value,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, actor_id, actor_name, action, target, detail, at
         FROM audit_events
         WHERE ($1::bigint IS NULL OR id < $1)
           AND ($2::bigint IS NULL OR actor_id = $2)
           AND ($3::text   IS NULL OR target ILIKE '%' || $3 || '%')
           AND ($4::timestamptz IS NULL OR at >= $4)
         ORDER BY id DESC
         LIMIT $5",
    )
    .bind(q.before)
    .bind(q.actor_id)
    .bind(q.target.as_deref())
    .bind(q.since)
    .bind(limit)
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing audit events", &e))?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(id, actor_id, actor_name, action, target, detail, at)| AuditView {
                    id,
                    actor_id,
                    actor_name,
                    action,
                    target,
                    detail,
                    at,
                },
            )
            .collect(),
    ))
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
#[serde(deny_unknown_fields)]
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
    /// `None` for a budget that caps only spend.
    tokens_total: Option<i64>,
    tokens_used: i64,
    cost_total_micros: Option<i64>,
    cost_used_micros: i64,
    window: String,
    window_start: chrono::DateTime<chrono::Utc>,
}

async fn list_budgets(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<Vec<BudgetView>>, ApiError> {
    // `tokens_total` is `Option` because the column is nullable: a budget may
    // cap only spend. Decoding it as `i64` made this route fail outright the
    // moment anyone created one — the whole listing, not just that row.
    type BudgetRow = (
        i64,
        String,
        Option<i64>,
        i64,
        Option<i64>,
        i64,
        String,
        chrono::DateTime<chrono::Utc>,
    );
    let rows: Vec<BudgetRow> = sqlx::query_as(
        "SELECT b.principal_id, p.name, b.tokens_total, b.tokens_used, \
         b.cost_total_micros, b.cost_used_micros, b.budget_window, b.window_start
         FROM budgets b JOIN principals p ON p.id = b.principal_id
         ORDER BY b.principal_id",
    )
    .fetch_all(&ctx.pool)
    .await
    .map_err(|e| db_error("listing budgets", &e))?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(
                    principal_id,
                    principal,
                    tokens_total,
                    tokens_used,
                    cost_total_micros,
                    cost_used_micros,
                    window,
                    window_start,
                )| {
                    BudgetView {
                        principal_id,
                        principal,
                        tokens_total,
                        tokens_used,
                        cost_total_micros,
                        cost_used_micros,
                        window,
                        window_start,
                    }
                },
            )
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutBudget {
    /// Either cap, or both. At least one is required — a budget with neither
    /// limits nothing and would be a silent no-op.
    #[serde(default)]
    tokens_total: Option<i64>,
    /// Micro-units of whatever currency `models` were priced in. Integer in the
    /// smallest unit anyone quotes, so there is no rounding mode to get wrong.
    #[serde(default)]
    cost_total_micros: Option<i64>,
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
    if body.tokens_total.is_none() && body.cost_total_micros.is_none() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "a budget needs tokens_total, cost_total_micros, or both; one with neither \
             limits nothing",
        ));
    }
    if body.tokens_total.is_some_and(|t| t <= 0) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "tokens_total must be a positive number of tokens",
        ));
    }
    if body.cost_total_micros.is_some_and(|c| c <= 0) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "cost_total_micros must be a positive amount",
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
        "INSERT INTO budgets (principal_id, tokens_total, cost_total_micros, budget_window)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (principal_id)
         DO UPDATE SET tokens_total = EXCLUDED.tokens_total,
                        cost_total_micros = EXCLUDED.cost_total_micros,
                        budget_window = EXCLUDED.budget_window,
                        updated_at = now()",
    )
    .bind(principal_id)
    .bind(body.tokens_total)
    .bind(body.cost_total_micros)
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
#[serde(deny_unknown_fields)]
struct ReconcileCountWire {
    principal_id: u64,
    requests: u64,
    tokens: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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
            let consecutive = ctx
                .snapshot_rebuild_failures
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            // The push half of the counter below. This one is worth waking
            // somebody for: the write committed, so the database and the
            // snapshot every proxy is serving have diverged, and nothing
            // reconciles them until a later rebuild happens to succeed.
            ctx.webhook
                .send(crate::webhook::Event::SnapshotRebuildFailed {
                    error: e.to_string(),
                    consecutive,
                });
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

/// How long per-request rows are kept before being folded into hourly
/// buckets. See `migrations/0023_usage_retention.sql` for why the two
/// granularities exist at all.
const RAW_RETENTION_DAYS: i64 = 90;

/// Fold everything older than the retention window into hourly buckets and
/// delete the raw rows it came from.
///
/// One transaction, and the order inside it is the whole correctness
/// argument: summarise, then delete exactly what was summarised, atomically.
/// Deleting first would lose data outright, and doing the two in separate
/// transactions would lose whatever arrived in between — a request written
/// after the `INSERT ... SELECT` read the range but before the `DELETE`
/// removed it would vanish having been counted nowhere.
///
/// `ON CONFLICT DO UPDATE` rather than plain insert because this is not
/// assumed to run exactly once per hour. A control plane that was down for a
/// day rolls up several windows on its next tick, and a retry after a
/// partial failure must not double-count; adding to the existing bucket
/// makes the operation converge on the same totals however many times it
/// runs.
async fn roll_up_and_prune_usage(pool: &PgPool) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(RAW_RETENTION_DAYS);

    let rolled = sqlx::query(
        // Keyed by the name the request was billed under, not the model id.
        // The id is NULL once the model is deleted, and it is part of this
        // table's primary key — so keying on it would fail the whole batch the
        // first time retention met a request served by a since-deleted model,
        // which is exactly what a registration service expiring a lease
        // produces. The name is also the better key: it does not merge two
        // different models that happened to reuse an id.
        "INSERT INTO usage_rollup_hourly (
             hour, provider_model_id, model_name, principal_id, requests, upstream_errors,
             refused_authorisation, refused_rate_limit, refused_budget, refused_no_backend,
             prompt_tokens, completion_tokens, cost_micros, unpriced_requests,
             duration_ms_sum, duration_ms_count)
         SELECT date_trunc('hour', at), min(provider_model_id), coalesce(model_name, '(unknown)'),
                principal_id,
                count(*),
                count(*) FILTER (WHERE refusal IS NULL AND status >= 400),
                count(*) FILTER (WHERE refusal = 'authorisation'),
                count(*) FILTER (WHERE refusal = 'rate_limit'),
                count(*) FILTER (WHERE refusal = 'budget'),
                count(*) FILTER (WHERE refusal = 'no_backend'),
                COALESCE(sum(prompt_tokens), 0),
                COALESCE(sum(completion_tokens), 0),
                COALESCE(sum(cost_micros), 0),
                count(*) FILTER (WHERE cost_micros IS NULL),
                COALESCE(sum(duration_ms), 0),
                count(*) FILTER (WHERE duration_ms IS NOT NULL)
         FROM usage_events
         WHERE at < $1
         GROUP BY 1, coalesce(model_name, '(unknown)'), principal_id
         ON CONFLICT (hour, model_name, principal_id) DO UPDATE SET
             requests              = usage_rollup_hourly.requests + EXCLUDED.requests,
             upstream_errors       = usage_rollup_hourly.upstream_errors + EXCLUDED.upstream_errors,
             refused_authorisation = usage_rollup_hourly.refused_authorisation
                                     + EXCLUDED.refused_authorisation,
             refused_rate_limit    = usage_rollup_hourly.refused_rate_limit
                                     + EXCLUDED.refused_rate_limit,
             refused_budget        = usage_rollup_hourly.refused_budget + EXCLUDED.refused_budget,
             refused_no_backend    = usage_rollup_hourly.refused_no_backend
                                     + EXCLUDED.refused_no_backend,
             prompt_tokens         = usage_rollup_hourly.prompt_tokens + EXCLUDED.prompt_tokens,
             completion_tokens     = usage_rollup_hourly.completion_tokens
                                     + EXCLUDED.completion_tokens,
             cost_micros           = usage_rollup_hourly.cost_micros + EXCLUDED.cost_micros,
             unpriced_requests     = usage_rollup_hourly.unpriced_requests
                                     + EXCLUDED.unpriced_requests,
             duration_ms_sum       = usage_rollup_hourly.duration_ms_sum + EXCLUDED.duration_ms_sum,
             duration_ms_count     = usage_rollup_hourly.duration_ms_count
                                     + EXCLUDED.duration_ms_count",
    )
    .bind(cutoff)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let pruned = sqlx::query("DELETE FROM usage_events WHERE at < $1")
        .bind(cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    tx.commit().await?;
    Ok((rolled, pruned))
}

/// The roll-up, reachable from the integration test that pins it lossless
/// and idempotent. Those are properties of the SQL, and the SQL is what a
/// unit test cannot reach — this is a thin door onto it rather than a second
/// implementation the test could pass while production failed.
pub async fn roll_up_and_prune_usage_for_test(pool: &PgPool) -> Result<(u64, u64), sqlx::Error> {
    roll_up_and_prune_usage(pool).await
}

/// Run the roll-up on a slow timer.
///
/// Hourly rather than on the snapshot-rebuild tick: this touches only rows
/// months old, so running it more often is pure cost, and its failure is not
/// urgent — the next tick catches up whatever the last one missed.
/// Probe every provider on a schedule: record what is healthy, degrade what is
/// not, and remove dynamic providers whose absence has outlasted the grace
/// window.
///
/// One `GET /v1/models` per provider answers both questions that matter —
/// whether it is alive, and whether it is still serving what the registry says
/// — and it costs one call however many models ride on it.
///
/// This runs on the control plane, not the proxies: once, rather than once per
/// replica, and nowhere near the request path.
pub fn spawn_provider_sweep(
    pool: PgPool,
    client: std::sync::Arc<crate::upstream::Upstream>,
    interval: std::time::Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match crate::control::registry_agent::sweep(&pool, &client).await {
                Ok(r) => {
                    if !r.mismatched.is_empty() {
                        // Loudest of the three, because it is the one an
                        // operator cannot see any other way: the host is up,
                        // the probe is green, and it is serving the wrong
                        // thing.
                        tracing::warn!(
                            providers = ?r.mismatched,
                            "provider is serving models other than those registered on it"
                        );
                    }
                    if !r.unreachable.is_empty() {
                        tracing::info!(providers = ?r.unreachable, "provider unreachable; degraded");
                    }
                    if !r.deleted.is_empty() {
                        tracing::warn!(
                            providers = ?r.deleted,
                            "removed dynamic providers whose absence outlasted the grace window"
                        );
                    }
                    if r.models_added > 0 || r.models_removed > 0 {
                        tracing::info!(
                            added = r.models_added,
                            removed = r.models_removed,
                            "reconciled models on dynamic providers"
                        );
                    }
                }
                // Warn, never fail. A sweep that cannot run costs freshness;
                // taking the control plane down over it would cost service.
                Err(e) => tracing::warn!(error = %e, "provider sweep failed; will retry next tick"),
            }
        }
    });
}

pub fn spawn_usage_retention(pool: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match roll_up_and_prune_usage(&pool).await {
                Ok((0, 0)) => {}
                Ok((rolled, pruned)) => tracing::info!(
                    rolled_buckets = rolled,
                    pruned_rows = pruned,
                    retention_days = RAW_RETENTION_DAYS,
                    "folded old usage rows into hourly buckets"
                ),
                // Warn, never fail: losing a roll-up costs disk, and taking
                // the control plane down over it would cost service.
                Err(e) => tracing::warn!(error = %e, "usage roll-up failed; will retry next tick"),
            }
        }
    });
}

/// `GET /openapi.json`: the machine-readable route list.
///
/// Unauthenticated, deliberately. It describes the shape of the API and
/// contains no data — and a spec you need a session to read is a spec nobody
/// generates a client from, which defeats the point of publishing one.
///
/// Served from a checked-in file rather than derived at runtime. The
/// derivation would have to reproduce every handler's parameters and
/// responses in a second form, which is the drift this is meant to prevent;
/// `tests/openapi.rs` compares the file against the router instead, so a
/// route added without an entry fails the build rather than shipping
/// undocumented.
async fn openapi_spec() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        include_str!("../../openapi.json"),
    )
}

/// `GET /docs`: Swagger UI over the spec above.
///
/// The page is a few lines because the heavy lifting is a CDN script — and
/// that is the one thing to know about it: an air-gapped deployment gets an
/// empty page here, while `/openapi.json` still works. Vendoring the bundle
/// would add half a megabyte to the binary for a convenience.
async fn openapi_ui() -> impl IntoResponse {
    axum::response::Html(
        // `r##` rather than `r#`: the page contains `"#` in `dom_id:"#ui"`,
        // which would close a single-hash raw string early.
        r##"<!doctype html><html><head><meta charset="utf-8">
<title>fastllm-proxy API</title>
<link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
</head><body><div id="ui"></div>
<script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
<script>SwaggerUIBundle({url:"/openapi.json",dom_id:"#ui"})</script>
</body></html>"##,
    )
}

/// Shared by every route gated on the proxy's own bootstrap token —
/// `/snapshot` and `/usage` alike. One implementation so a second, subtly
/// different bearer-check (a `==` that forgot why `constant_time_eq` exists,
/// say) can never be written for the route added after this one.
///
/// `/snapshot` discloses every key hash and usable upstream backend
/// credentials (see the schema comment on `providers.upstream_api_key`);
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
#[serde(deny_unknown_fields)]
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
    let mut duration_ms: Vec<Option<i32>> = Vec::with_capacity(submitted);
    let mut ttft_ms: Vec<Option<i32>> = Vec::with_capacity(submitted);
    let mut status: Vec<Option<i16>> = Vec::with_capacity(submitted);
    let mut requested_model: Vec<Option<String>> = Vec::with_capacity(submitted);
    let mut reported_cost: Vec<Option<i64>> = Vec::with_capacity(submitted);
    let mut usage_reported: Vec<bool> = Vec::with_capacity(submitted);
    let mut refusal: Vec<Option<String>> = Vec::with_capacity(submitted);
    for e in &body.events {
        principal_ids.push(e.principal_id as i64);
        models.push(e.model.clone());
        prompt_tokens.push(e.prompt_tokens as i64);
        completion_tokens.push(e.completion_tokens as i64);
        at.push(e.at);
        duration_ms.push(e.duration_ms.map(|v| v as i32));
        ttft_ms.push(e.ttft_ms.map(|v| v as i32));
        status.push(e.status.map(|v| v as i16));
        requested_model.push(e.requested_model.clone());
        reported_cost.push(e.cost_micros.map(|c| c.min(i64::MAX as u64) as i64));
        usage_reported.push(e.usage_reported);
        // Serialised through the enum rather than formatted ad hoc, so the
        // column can only ever hold names `usage::Refusal` knows.
        refusal.push(e.refusal.and_then(|r| {
            serde_json::to_value(r)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
        }));
    }

    // `UNNEST` turns the parallel arrays back into rows, positionally —
    // this is what lets one round trip insert a whole batch instead of one
    // query per event. The `JOIN` against `principals` (rather than a
    // `NOT NULL` foreign key the `INSERT` could violate) is what makes a row
    // naming an unknown principal silently absent from `RETURNING` instead of
    // failing the statement outright.
    //
    // `models` is a LEFT JOIN, deliberately. It used to be an inner one, which
    // dropped any event naming a model that no longer existed — reasonable
    // while models were deleted rarely and by hand, and wrong the moment
    // anything deletes them on a schedule: a proxy flushing a batch for a
    // model that expired mid-flush would lose those rows, and that is the
    // normal shape of a model swap, not an edge case. The traffic happened;
    // recording it is the only honest thing to do about it.
    //
    // A row that misses gets no `provider_model_id` and no price, so its cost is NULL —
    // unknown rather than a confident zero — and `model_name` still says what
    // the caller asked for.
    let accepted_rows: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
        "WITH input AS (
            SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::bigint[], $4::bigint[], \
                $5::timestamptz[], $6::int[], $7::int[], $8::smallint[], $9::text[], \
                $10::bigint[], $11::boolean[], $12::text[])
                AS t(principal_id, model_name, prompt_tokens, completion_tokens, at,
                     duration_ms, ttft_ms, status, requested_model, reported_cost,
                     usage_reported, refusal)
         )
         INSERT INTO usage_events (principal_id, provider_model_id, model_name, provider_name,
                                   prompt_tokens, completion_tokens, at,
                                   duration_ms, ttft_ms, status, requested_model, usage_reported,
                                   refusal, cost_micros)
         SELECT i.principal_id, m.id, i.model_name, pr.name,
                i.prompt_tokens, i.completion_tokens, i.at,
                i.duration_ms, i.ttft_ms, i.status, i.requested_model, i.usage_reported,
                i.refusal,
                -- Computed here, from the price at the time the request
                -- happened, and stored. Deriving it on read would let a later
                -- price change silently rewrite history; what a request cost is
                -- a fact about when it ran.
                --
                -- NULL when the model is unpriced, so unpriced is visible
                -- rather than looking free.
                --
                -- The provider's own figure wins where it gave one: it is the
                -- amount actually billed, it already accounts for cache
                -- discounts and for a routed alias serving a different model
                -- per request, and it does not go stale when a price changes.
                -- The configured price is the fallback, not the source.
                --
                -- Rounded, not truncated. Integer division truncates toward
                -- zero, and a small request often costs single-digit
                -- micro-units — truncating every one of them undercounts
                -- systematically rather than symmetrically.
                COALESCE(
                    i.reported_cost,
                    -- An event with no token counts has no computable cost.
                    -- Without this arm the arithmetic below runs on the
                    -- zeroes that stand in for counts nobody reported, and
                    -- yields a confident 0 -- which reads as a request that
                    -- was priced and free rather than one whose cost is
                    -- unknown. The UI already distinguishes unpriced from
                    -- zero; this keeps that distinction true at the source.
                    CASE WHEN NOT i.usage_reported
                         THEN NULL
                         WHEN m.input_price_per_mtok IS NULL AND m.output_price_per_mtok IS NULL
                         THEN NULL
                         ELSE ((i.prompt_tokens     * COALESCE(m.input_price_per_mtok, 0)
                              + i.completion_tokens * COALESCE(m.output_price_per_mtok, 0))
                              + 500000) / 1000000
                    END
                )
         FROM input i
         JOIN principals p ON p.id = i.principal_id
         LEFT JOIN provider_models m ON m.name = i.model_name
         LEFT JOIN providers pr ON pr.id = m.provider_id
         RETURNING id, principal_id, prompt_tokens, completion_tokens, COALESCE(cost_micros, 0)",
    )
    .bind(&principal_ids)
    .bind(&models)
    .bind(&prompt_tokens)
    .bind(&completion_tokens)
    .bind(&at)
    .bind(&duration_ms)
    .bind(&ttft_ms)
    .bind(&status)
    .bind(&requested_model)
    .bind(&reported_cost)
    .bind(&usage_reported)
    .bind(&refusal)
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
async fn apply_usage_to_budgets(pool: &PgPool, accepted_rows: &[(i64, i64, i64, i64, i64)]) {
    // Tokens and cost accrue together, in one statement per principal: they
    // describe the same requests, and updating them separately would leave a
    // window where a budget had spent the money but not the tokens.
    let mut totals: HashMap<i64, (i64, i64)> = HashMap::new();
    for (_id, principal_id, prompt_tokens, completion_tokens, cost_micros) in accepted_rows {
        let entry = totals.entry(*principal_id).or_insert((0, 0));
        entry.0 += prompt_tokens + completion_tokens;
        entry.1 += cost_micros;
    }
    for (principal_id, (total, cost)) in totals {
        if total <= 0 && cost <= 0 {
            continue;
        }
        if let Err(e) = sqlx::query(
            "UPDATE budgets SET tokens_used = tokens_used + $2, \
             cost_used_micros = cost_used_micros + $3, updated_at = now()
             WHERE principal_id = $1",
        )
        .bind(principal_id)
        .bind(total)
        .bind(cost)
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
#[serde(deny_unknown_fields)]
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
/// decide *what* they may do. That is `check_permission`'s job, run as an
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
    // A principal API key is accepted as well as a session cookie, because a
    // registration agent on a GPU host is not a person at a browser. Giving it
    // a password so it could `POST /login` would put one on every GPU host,
    // which is the thing having machine credentials exists to avoid.
    //
    // This authenticates only. What a key may *do* is still decided by
    // `check_permission` per route, so a key without `config:write` gets a 403
    // from every admin route that mutates configuration, exactly as before.
    // Only when there is no session cookie. A request carrying both — a
    // browser session plus some unrelated Authorization header — must still
    // authenticate as the session it has, and treating a bearer token as
    // authoritative whenever it appears turned every such request into a 401.
    let cookie = session_cookie(&headers);
    if let (None, Some(bearer)) = (
        &cookie,
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ")),
    ) {
        let hash = hash_key(bearer.trim()).to_vec();
        let principal_id: Option<i64> = sqlx::query_scalar(
            "SELECT principal_id FROM api_keys \
              WHERE hash = $1 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(hash)
        .fetch_optional(&ctx.pool)
        .await
        .map_err(|e| db_error("authenticating a key", &e))?;
        let Some(principal_id) = principal_id else {
            return Err(api_error(
                StatusCode::UNAUTHORIZED,
                "invalid or expired key",
            ));
        };
        request
            .extensions_mut()
            .insert(AdminPrincipal(principal_id));
        return Ok(next.run(request).await);
    }

    let Some(token) = cookie else {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "no session cookie or bearer key",
        ));
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
    // the only place a resolved principal id has to go. The permission
    // extractors below read it back out inside the matched handler's own
    // extractor chain.
    request
        .extensions_mut()
        .insert(AdminPrincipal(principal_id));
    Ok(next.run(request).await)
}

/// The authenticated caller of an `/admin/*` request — `require_session`'s
/// output and `check_permission`'s input. A newtype rather than a bare
/// `i64` so it cannot be confused with some other `i64` extension a future
/// route adds, and so it is not `Clone`-and-forgettable the way threading a
/// raw id through would be.
#[derive(Debug, Clone, Copy)]
struct AdminPrincipal(i64);

/// Extractable directly, so a handler that only needs to know *who* is asking
/// does not have to go through a permission marker to find out.
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for AdminPrincipal {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AdminPrincipal>()
            .copied()
            .ok_or_else(|| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "no authenticated principal on this request".to_string(),
                )
            })
    }
}

/// Every permission `/admin/*` routes check against, named for the
/// `permissions.verb` column value each one maps to. Deliberately only
/// these four (plus `model:invoke`, irrelevant here — that one gates the
/// *data* plane, resolved once per snapshot build in
/// `control::build::flatten_grants`, not checked per admin request): the
/// vocabulary `migrations/0001_init.sql` already seeds, not a parallel
/// permission scheme invented for this file. A route that does not fit any
/// of the three specific verbs (`key:create`, `key:revoke`) falls back to
/// `config:write` — the schema has no finer-grained permission for
/// "manage principals" or "manage frontend models" than that, and inventing
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

/// The permission check behind the four extractors below.
///
/// An extractor, not a second `from_fn` middleware layer: which permission a
/// request needs depends on which route matched, and axum's `from_fn` layers
/// run *before* routing has picked a handler, so they cannot see that. Taking
/// a `RequireRead`/`RequireKeyCreate`/`RequireKeyRevoke`/`RequireConfigWrite`
/// as a handler argument runs this once axum already knows which handler —
/// and therefore which permission — applies, and the `Rejection = ApiError`
/// turns a missing grant into 403 automatically, the same way a bad
/// `Path<i64>` already turns into 400 without every handler writing that
/// check by hand.
async fn check_permission(parts: &Parts, state: &Ctx, verb: &str) -> Result<(), ApiError> {
    // `require_session` (the `from_fn` layer every `/admin/*` route is
    // wrapped in) always runs first and always inserts this — its absence
    // here would mean the extractor is used on a route that is not behind
    // that layer, a wiring bug rather than a caller error, so a 500 rather
    // than a 401/403 correctly signals that rather than being mistaken for an
    // unauthenticated request.
    let Some(AdminPrincipal(principal_id)) = parts.extensions.get::<AdminPrincipal>().copied()
    else {
        tracing::error!(
            "an admin permission check ran with no AdminPrincipal in request extensions; \
             is this route mounted outside require_session?"
        );
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "authorisation check misconfigured; see server logs",
        ));
    };
    let allowed = principal_has_permission(&state.pool, principal_id, verb)
        .await
        .map_err(|e| db_error("checking admin permission", &e))?;
    if allowed {
        Ok(())
    } else {
        Err(api_error(
            StatusCode::FORBIDDEN,
            format!("principal {principal_id} lacks the {verb:?} permission"),
        ))
    }
}

// One extractor per permission an admin route can require. Written out four
// times rather than generated from a marker type or a macro: each is six
// lines, and spelling the permission at the definition is what makes
// `RequireConfigWrite` findable by grep. Unit structs, so a test that calls a
// handler directly names the permission it is standing in for.

struct RequireRead;

impl FromRequestParts<Ctx> for RequireRead {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Ctx) -> Result<Self, Self::Rejection> {
        check_permission(parts, state, admin_permission::READ)
            .await
            .map(|()| Self)
    }
}

struct RequireKeyCreate;

impl FromRequestParts<Ctx> for RequireKeyCreate {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Ctx) -> Result<Self, Self::Rejection> {
        check_permission(parts, state, admin_permission::KEY_CREATE)
            .await
            .map(|()| Self)
    }
}

struct RequireKeyRevoke;

impl FromRequestParts<Ctx> for RequireKeyRevoke {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Ctx) -> Result<Self, Self::Rejection> {
        check_permission(parts, state, admin_permission::KEY_REVOKE)
            .await
            .map(|()| Self)
    }
}

/// Registering a provider needs a credential, not a permission.
///
/// There is no RBAC on providers. A provider is an endpoint and a credential
/// for reaching it; the credential is the provider's own (an OpenRouter key, a
/// vLLM server's auth), defined by the provider rather than by us.
///
/// Registering one is not an exposure, which is what makes this safe: a
/// provider model learned from a registered host reaches nobody until an
/// operator points a frontend model at it, and *that* is where access is
/// granted. Gating registration behind its own verb would guard a door that
/// opens onto nothing.
///
/// `require_session` has already authenticated the caller — a session or a
/// principal API key — so reaching this handler is the whole check.
struct RequireProviderRegister;

impl FromRequestParts<Ctx> for RequireProviderRegister {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &Ctx) -> Result<Self, Self::Rejection> {
        if parts.extensions.get::<AdminPrincipal>().is_some() {
            return Ok(Self);
        }
        tracing::error!(
            "provider registration ran with no AdminPrincipal; is the route mounted \
             outside require_session?"
        );
        Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "authorisation check misconfigured; see server logs",
        ))
    }
}

struct RequireConfigWrite;

impl FromRequestParts<Ctx> for RequireConfigWrite {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Ctx) -> Result<Self, Self::Rejection> {
        check_permission(parts, state, admin_permission::CONFIG_WRITE)
            .await
            .map(|()| Self)
    }
}

/// Record every configuration change, in one place.
///
/// A layer rather than a call in each handler, and that is the point: a
/// hand-wired audit trail records the mutations somebody remembered to wire,
/// which drifts the moment a route is added. Everything that is not a `GET`
/// under `/admin/*` passes through here, so a new endpoint is audited before it
/// is written.
///
/// What that costs is detail — this records *that* a principal's roles were
/// changed and by whom, not which role. The trade is deliberate: complete and
/// coarse beats detailed and full of holes, and the request that follows in the
/// application log carries the rest.
///
/// Reads are not recorded. `GET /admin/keys` returns prefixes, never secrets,
/// and auditing every list call would bury the changes in noise. A handful of
/// routes are `POST` only because they take a body — see `is_read_only`.
///
/// The body is never captured. It carries passwords and upstream credentials,
/// and an audit row is read by more people than the thing it describes.
/// `POST /admin/roles`: create a role to hang permissions on.
///
/// The gap this closes: permissions attach to roles, not to principals, so
/// "this app may call these two models and nothing else" is unexpressible
/// without a role to put it on. Without this the only scoping available was
/// the seeded `inference` role, which grants `model/*` — every model,
/// including the paid ones — and handing that to a third-party app is how a
/// provider bill arrives unexpectedly.
async fn post_role(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(body): Json<NewRole>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "a role needs a name".to_string(),
        ));
    }
    let id: i64 =
        sqlx::query_scalar("INSERT INTO roles (name, description) VALUES ($1, $2) RETURNING id")
            .bind(name)
            .bind(body.description.unwrap_or_default())
            .fetch_one(&ctx.pool)
            .await
            .map_err(|e| {
                if is_unique_violation(&e) {
                    api_error(
                        StatusCode::CONFLICT,
                        format!("a role named {name:?} already exists"),
                    )
                } else {
                    db_error("creating the role", &e)
                }
            })?;

    // No `refresh` here: a role with no permissions and no holders changes
    // nothing a proxy can see. The grant that follows publishes.
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "name": name })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRole {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

/// `DELETE /admin/roles/{name}`: remove a role nobody holds.
///
/// Refused while any principal still holds it, rather than cascading. A
/// cascade here silently removes access from every holder at once, and the
/// symptom — a fleet of callers all getting 403 — arrives long after the click
/// that caused it. Revoking the role from its principals first makes that
/// consequence something an operator chose rather than discovered.
async fn delete_role(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let holders: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM principal_roles pr JOIN roles r ON r.id = pr.role_id
         WHERE r.name = $1",
    )
    .bind(&name)
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| db_error("counting role holders", &e))?;
    if holders > 0 {
        return Err(api_error(
            StatusCode::CONFLICT,
            format!(
                "{holders} principal(s) still hold {name:?}; revoke it from them first — \
                 deleting it here would take their access away all at once"
            ),
        ));
    }
    let done = sqlx::query("DELETE FROM roles WHERE name = $1")
        .bind(&name)
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("deleting the role", &e))?;
    if done.rows_affected() == 0 {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            format!("no role named {name:?}"),
        ));
    }
    refresh(&ctx).await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /admin/config`: what this process was started with.
///
/// Read-only. Every value here is a CLI flag or a build feature, and changing
/// one is a deploy — but a settings screen that *guesses* them is worse than
/// none, because it shows a default that a `--config-poll 30` deployment does
/// not have.
async fn get_config(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
    caller: AdminPrincipal,
) -> Result<Json<serde_json::Value>, ApiError> {
    let unpriced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_models WHERE input_price_per_mtok IS NULL",
    )
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| db_error("counting unpriced models", &e))?;
    let cached: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_models WHERE cache_ttl_seconds IS NOT NULL AND cache_ttl_seconds > 0",
    )
    .fetch_one(&ctx.pool)
    .await
    .map_err(|e| db_error("counting cache-enabled models", &e))?;
    let models: i64 = sqlx::query_scalar("SELECT count(*) FROM provider_models")
        .fetch_one(&ctx.pool)
        .await
        .map_err(|e| db_error("counting models", &e))?;

    // Who is asking. The session cookie is HttpOnly, so the browser cannot
    // read its own identity back after a reload — the UI showed the operator's
    // name until the first refresh and "signed in" forever after. This is the
    // cheapest place to answer it: the caller is already authenticated here.
    let you: Option<String> = sqlx::query_scalar("SELECT name FROM principals WHERE id = $1")
        .bind(caller.0)
        .fetch_optional(&ctx.pool)
        .await
        .map_err(|e| db_error("reading the calling principal", &e))?;

    let d = &*ctx.deployment;
    Ok(Json(serde_json::json!({
        "you": you,
        "role": d.role,
        "version": d.version,
        "tls": ctx.tls_enabled,
        "uptime_seconds": ctx.started_at.elapsed().as_secs(),
        "config_poll_seconds": d.config_poll_seconds,
        "health_report_interval_seconds": d.health_report_interval_seconds,
        "cache_max_entries": d.cache_max_entries,
        "cache_max_bytes": d.cache_max_bytes,
        "otel_endpoint": d.otel_endpoint,
        "otel_sample_one_in": d.otel_sample_one_in,
        "policy": d.policy,
        "webhook_configured": d.webhook_configured,
        "webhook_signed": d.webhook_signed,
        "classifier_tier1": d.classifier_tier1,
        "classifier_tier2": d.classifier_tier2,
        "session_ttl_hours": crate::control::auth::SESSION_TTL_HOURS,
        "snapshot_rebuild_failures": ctx.snapshot_rebuild_failures.load(Ordering::Relaxed),
        "snapshot_version": ctx.cache.current_snapshot().version,
        "models": models,
        "models_unpriced": unpriced,
        "models_cached": cached,
        // What the UI keys its deployment screen off. A boolean rather than
        // the resource reference: a deployment nobody operates should not
        // learn a namespace/name it cannot use.
        "operator_managed": ctx.operator.is_some(),
    })))
}

/// `POST /admin/snapshot/rebuild`: publish a freshly built snapshot now.
///
/// Every mutating route already does this; this is the manual one, for when
/// something outside the admin API changed the database — a migration, a
/// `sync-prices` run against Postgres directly, an operator with psql.
async fn rebuild_snapshot(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
) -> Result<Json<serde_json::Value>, ApiError> {
    refresh(&ctx).await;
    let snapshot = ctx.cache.current_snapshot();
    // Reported rather than assumed: `refresh` deliberately does not fail the
    // request that triggered it (see its doc comment), so "the route answered
    // 200" is not the same as "a new snapshot was published".
    Ok(Json(serde_json::json!({
        "snapshot_version": snapshot.version,
        "rebuild_failures": ctx.snapshot_rebuild_failures.load(Ordering::Relaxed),
    })))
}

/// `GET /admin/deployment`: the `FastllmProxy` running this process.
///
/// 404 when nothing operates this deployment, which is the same answer the UI
/// uses to hide the screen — `GET /admin/config`'s `operator_managed` is the
/// cheap version of this question, asked once at load.
async fn get_deployment(
    State(ctx): State<Ctx>,
    _perm: RequireRead,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(op) = ctx.operator.as_ref() else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "this deployment is not managed by the fastllm operator",
        ));
    };
    let cr = op.get().await.map_err(|e| {
        // The API server's own message names the missing RBAC rule when that
        // is the problem, which is the usual problem.
        api_error(
            StatusCode::BAD_GATEWAY,
            format!("reading the FastllmProxy: {e}"),
        )
    })?;
    // Deliberately a projection, not the whole resource: the CR carries
    // Secret *references*, and echoing the shape of a deployment's secret
    // wiring into a UI response is a detail that has no business being there.
    Ok(Json(serde_json::json!({
        "namespace": op.namespace,
        "name": op.name,
        "image": cr.pointer("/spec/image"),
        "proxy": {
            "replicas": cr.pointer("/spec/proxy/replicas"),
            "policy": cr.pointer("/spec/proxy/policy"),
            "upstream_timeout": cr.pointer("/spec/proxy/upstreamTimeout"),
            "workers": cr.pointer("/spec/proxy/workers"),
            "pool_max_idle": cr.pointer("/spec/proxy/poolMaxIdle"),
            "service_type": cr.pointer("/spec/proxy/serviceType"),
            "autoscaling": cr.pointer("/spec/proxy/autoscaling"),
        },
        "status": cr.get("status"),
    })))
}

/// `PATCH /admin/deployment`: change the shape of this deployment.
///
/// The write half of the screen. Everything it can change is a field the
/// operator already knows how to roll out safely — an image change is
/// sequenced across the two planes, a replica change is a scale — and the
/// allowlist is enforced by the type, not by this handler: see
/// `control::k8s::DeploymentEdit` for what is deliberately not in it.
async fn patch_deployment(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
    Json(edit): Json<crate::control::k8s::DeploymentEdit>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(op) = ctx.operator.as_ref() else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "this deployment is not managed by the fastllm operator",
        ));
    };
    let Some(patch) = edit.into_patch() else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "no changes: send at least one field",
        ));
    };
    let cr = op.patch_spec(patch).await.map_err(|e| {
        api_error(
            StatusCode::BAD_GATEWAY,
            format!("patching the FastllmProxy: {e}"),
        )
    })?;
    // No `refresh(&ctx)`: nothing here touches the database or the snapshot.
    // What this changed is a Kubernetes resource, and the operator — not this
    // process — is what acts on it.
    Ok(Json(serde_json::json!({
        "image": cr.pointer("/spec/image"),
        "generation": cr.pointer("/metadata/generation"),
    })))
}

/// `POST /admin/sessions/revoke-all`: log everybody out, including the caller.
///
/// The blunt instrument for a suspected stolen cookie. It does not spare the
/// caller: a route that kept its own session alive would be one an attacker
/// who already has a session could use to lock everyone else out.
async fn revoke_all_sessions(
    State(ctx): State<Ctx>,
    _perm: RequireConfigWrite,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deleted = sqlx::query("DELETE FROM sessions")
        .execute(&ctx.pool)
        .await
        .map_err(|e| db_error("revoking sessions", &e))?
        .rows_affected();
    Ok(Json(
        serde_json::json!({ "revoked": deleted, "includes_caller": true }),
    ))
}

/// Routes that are `POST` because they take a body, not because they change
/// anything.
///
/// The audit trail's value comes from every row being a change; a dry run or an
/// evaluation recorded as one dilutes it exactly the way auditing `GET` would.
/// A closed list rather than a marker on the handler, so it sits next to the
/// rule it is an exception to.
fn is_read_only(path: &str) -> bool {
    matches!(
        path,
        "/admin/routing/dry-run" | "/admin/prompt-classes/evaluate"
    )
}

async fn audit_changes(
    State(ctx): State<Ctx>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    // `require_session` runs first and always inserts this.
    let actor = request
        .extensions()
        .get::<AdminPrincipal>()
        .copied()
        .map(|AdminPrincipal(id)| id);

    let response = next.run(request).await;

    if method == axum::http::Method::GET || is_read_only(&path) {
        return response;
    }
    // Only changes that actually happened. A 403 or a 400 is an attempt, and
    // recording it as a change would make the trail lie in the direction that
    // matters most.
    if !response.status().is_success() {
        return response;
    }

    let actor_id = actor.unwrap_or(0);
    let actor_name: String = sqlx::query_scalar("SELECT name FROM principals WHERE id = $1")
        .bind(actor_id)
        .fetch_optional(&ctx.pool)
        .await
        .ok()
        .flatten()
        // A principal deleted between acting and this lookup, or a route
        // reached without a session. The id still identifies them.
        .unwrap_or_else(|| format!("principal:{actor_id}"));

    if let Err(e) = sqlx::query(
        "INSERT INTO audit_events (actor_id, actor_name, action, target, detail)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(actor)
    .bind(&actor_name)
    .bind(method.as_str())
    .bind(&path)
    .bind(serde_json::json!({"status": response.status().as_u16()}))
    .execute(&ctx.pool)
    .await
    {
        // Never fails the request. Losing an audit row is serious; losing the
        // change as well would be worse, since an operator retrying a failed
        // grant would have no way to tell whether the first attempt applied.
        tracing::error!(
            error = %e,
            method = %method,
            %path,
            actor = %actor_name,
            "could not write an audit row; the change itself was applied"
        );
    }
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Process facts an operator can only otherwise learn by reading the
/// Deployment manifest.
///
/// Read-only, and deliberately so: these are flags a process was started with,
/// and changing them is a deploy. What the UI needs is to stop *guessing* them
/// — a settings screen that invents "5s" because that is the default lies the
/// moment somebody passes `--config-poll 30`.
#[derive(Clone, Serialize)]
pub struct Deployment {
    pub role: String,
    pub version: String,
    pub config_poll_seconds: u64,
    pub health_report_interval_seconds: u64,
    pub cache_max_entries: usize,
    pub cache_max_bytes: usize,
    pub otel_endpoint: Option<String>,
    pub otel_sample_one_in: u64,
    pub classifier_tier1: bool,
    pub classifier_tier2: bool,
    /// Which backend within a pool a request goes to.
    pub policy: String,
    /// Whether outbound notifications are configured, and whether they are
    /// signed. Never the URL or the secret: this endpoint is readable by any
    /// principal with `usage:read`, and a webhook URL is a capability —
    /// anyone who learns it can post to the receiver.
    pub webhook_configured: bool,
    pub webhook_signed: bool,
}

/// Eight parameters, which clippy dislikes. Each is a distinct dependency
/// this server needs and none is derivable from the others; bundling them
/// into a config struct would move the same list one line up and add a type
/// whose only purpose is to satisfy a lint.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    pool: PgPool,
    addr: SocketAddr,
    proxy_token: String,
    cache: Arc<dyn SnapshotSink>,
    key: Arc<EncryptionKey>,
    tls: Option<rustls::ServerConfig>,
    deployment: Deployment,
    webhook: Arc<crate::webhook::WebhookSender>,
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
        deployment: Arc::new(deployment),
        webhook,
        started_at: std::time::Instant::now(),
        // Two report intervals plus slack: a replica that misses one delivery
        // should not vanish from a UI, and one that has genuinely gone should
        // not linger.
        fleet: Arc::new(crate::health_report::store::Fleet::new(
            std::time::Duration::from_secs(30),
        )),
        // Detected once, at startup: whether an operator manages this
        // deployment cannot change without the pod being recreated by that
        // very operator.
        operator: crate::control::k8s::Operator::from_env().map(Arc::new),
    };
    if let Some(op) = &ctx.operator {
        tracing::info!(
            resource = %format!("{}/{}", op.namespace, op.name),
            "operator-managed: the deployment screen is available"
        );
    }
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
        .route(
            "/admin/a2a-agents",
            get(list_a2a_agents).post(post_a2a_agent),
        )
        .route(
            "/admin/a2a-agents/{id}",
            axum::routing::patch(patch_a2a_agent).delete(delete_a2a_agent),
        )
        .route(
            "/admin/mcp-servers",
            get(list_mcp_servers).post(post_mcp_server),
        )
        .route(
            "/admin/mcp-servers/{id}",
            axum::routing::patch(patch_mcp_server).delete(delete_mcp_server),
        )
        .route("/admin/provider-catalogue", get(list_provider_catalogue))
        .route("/admin/providers", get(list_providers))
        .route("/admin/providers/{id}", delete(delete_provider))
        .route("/admin/providers/register", post(post_provider_register))
        .route("/admin/provider-models", get(list_models).post(post_model))
        .route(
            "/admin/provider-models/{id}",
            axum::routing::patch(patch_model).delete(delete_model),
        )
        .route("/admin/provider-models/{id}/backends", post(post_backend))
        .route("/admin/backends/{id}", delete(delete_backend))
        .route(
            "/admin/frontend-models",
            get(list_virtual_models).post(post_frontend_model),
        )
        .route(
            "/admin/frontend-models/{id}",
            delete(delete_virtual_model).patch(patch_frontend_model),
        )
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
        .route("/admin/frontend-models/{id}/rules", post(post_rule))
        .route("/admin/rules/{id}", delete(delete_rule))
        .route("/admin/rules/{id}/targets", post(post_rule_target))
        .route("/admin/rule-targets/{id}", delete(delete_rule_target))
        .route(
            "/admin/frontend-models/{id}/defaults",
            post(post_default_target),
        )
        .route(
            "/admin/frontend-model-defaults/{id}",
            delete(delete_default_target),
        )
        .route("/admin/roles", get(list_roles).post(post_role))
        .route("/admin/roles/{name}", delete(delete_role))
        .route("/admin/audit", get(list_audit))
        .route("/admin/usage", get(usage_summary))
        .route("/admin/timeseries", get(timeseries))
        .route("/admin/fleet", get(list_fleet))
        .route("/admin/routing/dry-run", post(routing_dry_run))
        .route("/admin/prices/sync", post(sync_prices))
        .route("/admin/config", get(get_config))
        .route(
            "/admin/deployment",
            get(get_deployment).patch(patch_deployment),
        )
        .route("/admin/snapshot/rebuild", post(rebuild_snapshot))
        .route("/admin/sessions/revoke-all", post(revoke_all_sessions))
        .route(
            "/admin/roles/{name}/permissions",
            post(grant_permission).delete(revoke_permission),
        )
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
        // Order matters: layers run outermost-last, so `require_session` is
        // applied after this one and therefore runs *first*. That is what lets
        // the audit layer read the `AdminPrincipal` the session put in the
        // request's extensions.
        .layer(axum::middleware::from_fn_with_state(
            ctx.clone(),
            audit_changes,
        ))
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
        .route("/health-report", post(post_health_report))
        .route("/limits/reconcile", post(post_reconcile))
        .route("/healthz", get(healthz))
        .route("/openapi.json", get(openapi_spec))
        .route("/docs", get(openapi_ui));

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
            // comment on `providers.upstream_api_key`) and `/usage`
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
mod policy_tests {
    use super::*;

    /// The stored spelling is the one `--policy` takes, so a value typed in
    /// the UI, written to the column and read by a proxy cannot drift apart.
    #[test]
    fn a_known_policy_is_stored_in_the_canonical_spelling() {
        assert_eq!(
            validated_policy(Some(" lowest-latency ")).unwrap(),
            Some("lowest-latency".to_string())
        );
    }

    /// Absent and empty both mean "the deployment default", so a UI clearing
    /// the field does not have to send `null` specifically.
    #[test]
    fn absent_and_empty_both_mean_the_deployment_default() {
        assert_eq!(validated_policy(None).unwrap(), None);
        assert_eq!(validated_policy(Some("")).unwrap(), None);
        assert_eq!(validated_policy(Some("   ")).unwrap(), None);
    }

    /// A typo is refused here rather than accepted and silently ignored by
    /// every proxy that reads it — the proxy tolerates the unknown value, but
    /// an operator who typed it deserves to hear about it now.
    #[test]
    fn an_unknown_policy_is_refused_and_says_what_is_allowed() {
        let (status, body) = validated_policy(Some("cacheAffinity")).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body.0["error"].as_str().unwrap();
        assert!(message.contains("cache-affinity"), "{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gap that made the MCP gateway unusable end to end: every piece
    /// worked — schema, snapshot, routing, the screen — and the grant could
    /// not be written, because this allowlist had never heard of the verb.
    /// Found by granting it against a real deployment, not by a unit test.
    #[test]
    fn mcp_invoke_can_be_granted_and_is_scoped_like_model_invoke() {
        assert!(validate_grant("mcp:invoke", "mcp/*").is_ok());
        assert!(validate_grant("mcp:invoke", "mcp/github").is_ok());

        // A resource with the wrong prefix matches nothing at flatten time,
        // which on a permission matrix reads as access granted.
        assert!(validate_grant("mcp:invoke", "*").is_err());
        assert!(validate_grant("mcp:invoke", "model/github").is_err());

        // And the admin verbs still refuse a scope they cannot express.
        assert!(validate_grant("config:write", "mcp/github").is_err());
        assert!(validate_grant("config:write", "*").is_ok());
    }

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

    /// Two failures with one cause: a field the caller sent that the API did
    /// not model.
    ///
    /// `context_length` was PATCH-only, so `POST /admin/provider-models` accepted a
    /// request carrying it, returned 201, and created a model with no context
    /// window — silently, because serde drops unknown fields by default. It
    /// was found by reading a row after registering a real model, which is the
    /// only way it could be found.
    ///
    /// The field is settable here now, and every request struct in this module
    /// rejects what it does not understand, so the next one of these is a 400
    /// instead of a shrug.
    #[test]
    fn a_field_the_admin_api_does_not_model_is_refused_rather_than_dropped() {
        // Settable at creation, which is the specific bug.
        let ok: Result<NewModel, _> = serde_json::from_value(serde_json::json!({
            "name": "m", "context_length": 262144
        }));
        assert_eq!(ok.expect("valid").context_length, Some(262144));

        // And anything unmodelled is now an error rather than silence. A
        // plausible typo is the case that matters: it looks like it worked.
        for bad in [
            serde_json::json!({"name": "m", "contextLength": 262144}),
            serde_json::json!({"name": "m", "context_len": 262144}),
            serde_json::json!({"name": "m", "cache_ttl": 60}),
        ] {
            let r: Result<NewModel, _> = serde_json::from_value(bad.clone());
            assert!(r.is_err(), "{bad} was accepted and silently dropped");
        }
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
            // No cluster in a test, and no pretending there is one: the two
            // deployment routes answer 404 here, which is the same answer
            // every non-operator deployment gets.
            operator: None,
            fleet: Arc::new(crate::health_report::store::Fleet::new(
                std::time::Duration::from_secs(30),
            )),
            deployment: Arc::new(Deployment {
                role: "all".into(),
                version: "test".into(),
                config_poll_seconds: 5,
                health_report_interval_seconds: 10,
                cache_max_entries: 4096,
                cache_max_bytes: 64 * 1024 * 1024,
                otel_endpoint: None,
                otel_sample_one_in: 0,
                classifier_tier1: false,
                classifier_tier2: false,
                policy: "cache-affinity".into(),
                webhook_configured: false,
                webhook_signed: false,
            }),
            started_at: std::time::Instant::now(),
            webhook: Arc::new(crate::webhook::WebhookSender::disabled()),
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
            .track_prefix("provider_models", "name", "route-model-");
        let principal_name = unique_name("route-principal");
        let model_name = unique_name("route-model");

        let (status, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
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
            RequireConfigWrite,
            Path(principal_id),
            Json(RoleGrant {
                role: "inference".into(),
            }),
        )
        .await
        .unwrap();

        let (_, model) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let provider_model_id = model.0["id"].as_i64().unwrap();

        let upstream_credential = "sk-upstream-must-never-come-back";
        let (_, backend) = post_backend(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(provider_model_id),
            Json(NewBackend::openai(
                "http://route-test:8000/v1/",
                Some(upstream_credential.into()),
            )),
        )
        .await
        .unwrap();
        // `id` is the model's — the model is its own link to a provider since
        // migration 0029 — while the credential lives on the provider, so the
        // two ids below are deliberately different.
        let backend_id = backend.0["id"].as_i64().unwrap();
        let provider_id = backend.0["provider_id"].as_i64().unwrap();

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
        let listed = list_models(State(ctx.clone()), RequireRead).await.unwrap();
        let json = serde_json::to_string(&listed.0).unwrap();
        assert!(!json.contains(upstream_credential));
        assert!(json.contains("has_upstream_api_key"));

        // Encrypted at rest, not merely absent from the response.
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT upstream_api_key FROM providers WHERE id = $1")
                .bind(provider_id)
                .fetch_one(&ctx.pool)
                .await
                .unwrap();
        assert_ne!(stored, upstream_credential.as_bytes());

        revoke_role(
            State(ctx.clone()),
            RequireConfigWrite,
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

        delete_backend(State(ctx.clone()), RequireConfigWrite, Path(backend_id))
            .await
            .unwrap();
        delete_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(provider_model_id),
        )
        .await
        .unwrap();
        delete_principal(State(ctx.clone()), RequireConfigWrite, Path(principal_id))
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
            RequireConfigWrite,
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

        let listed = list_keys(State(ctx.clone()), RequireRead).await.unwrap();
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
            delete_model(State(ctx.clone()), RequireConfigWrite, Path(-1))
                .await
                .unwrap_err(),
            delete_backend(State(ctx.clone()), RequireConfigWrite, Path(-1))
                .await
                .unwrap_err(),
            delete_principal(State(ctx.clone()), RequireConfigWrite, Path(-1))
                .await
                .unwrap_err(),
            revoke_key(State(ctx.clone()), RequireKeyRevoke, Path(-1))
                .await
                .unwrap_err(),
        ] {
            assert_eq!(status, StatusCode::NOT_FOUND);
            let message = body.0["error"].as_str().unwrap();
            assert!(message.contains("-1"), "must name the id: {message}");
        }

        let (status, body) = grant_role(
            State(ctx.clone()),
            RequireConfigWrite,
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
            RequireConfigWrite,
            Path(-1),
            Json(NewBackend::openai("http://x:8000/v1", None)),
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("-1"));

        let (status, _) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
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
        let roles = list_roles(State(ctx), RequireRead).await.unwrap();
        let inference = roles.0.iter().find(|r| r.name == "inference").unwrap();
        assert!(inference
            .permissions
            .iter()
            .any(|p| p.verb == "model:invoke" && p.resource == "model/*"));
        let admin = roles.0.iter().find(|r| r.name == "admin").unwrap();
        assert!(admin.permissions.len() > inference.permissions.len());
    }

    /// Every frontend-model route, end to end: creating a frontend model, a
    /// rule on it, a target on that rule, and a default target all publish
    /// through the same `refresh()` -> `SnapshotSink::store_snapshot` path
    /// every other admin route uses — same invariant
    /// `every_mutating_route_publishes_through_the_one_write_path` pins for
    /// keys/models/principals, applied to the P1 tables.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn frontend_model_routes_publish_rules_and_targets_to_the_snapshot() {
        let (ctx, cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("provider_models", "name", "vm-route-primary-")
            .track_prefix("provider_models", "name", "vm-route-secondary-")
            .track_prefix("frontend_models", "name", "vm-route-canary-");
        let primary_name = unique_name("vm-route-primary");
        let secondary_name = unique_name("vm-route-secondary");

        let (_, primary) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: primary_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let primary_id = primary.0["id"].as_i64().unwrap();

        let (_, secondary) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: secondary_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let secondary_id = secondary.0["id"].as_i64().unwrap();

        let vm_name = unique_name("vm-route-canary");
        let (status, vm) = post_frontend_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewFrontendModel {
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
            RequireConfigWrite,
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
            RequireConfigWrite,
            Path(rule_id),
            Json(NewTarget {
                provider_model_id: primary_id,
                weight: 100,
                position: 0,
            }),
        )
        .await
        .unwrap();

        let _ = post_default_target(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(vm_id),
            Json(NewTarget {
                provider_model_id: secondary_id,
                weight: 100,
                position: 0,
            }),
        )
        .await
        .unwrap();

        let snap = cache.current_snapshot();
        let published = snap
            .frontend_models
            .get(&vm_name)
            .expect("the frontend model created over the admin API must be in the snapshot");
        assert_eq!(published.rules.len(), 1);
        assert_eq!(
            published.rules[0].conditions.caller.roles,
            ["canary".to_string()].into_iter().collect()
        );
        assert_eq!(published.rules[0].targets[0].model, primary_name);
        assert_eq!(published.default_targets[0].model, secondary_name);

        // `GET /admin/frontend-models` mirrors the same rows.
        let listed = list_virtual_models(State(ctx.clone()), RequireRead)
            .await
            .unwrap();
        let listed_vm = listed.0.iter().find(|v| v.name == vm_name).unwrap();
        assert_eq!(listed_vm.rules.len(), 1);
        assert_eq!(listed_vm.rules[0].targets[0].model, primary_name);
        assert_eq!(listed_vm.default_targets[0].model, secondary_name);

        // Deleting the frontend model cascades its rules and targets, and the
        // deletion reaches the published snapshot the same way creation did.
        delete_virtual_model(State(ctx.clone()), RequireConfigWrite, Path(vm_id))
            .await
            .unwrap();
        assert!(
            !cache
                .current_snapshot()
                .frontend_models
                .contains_key(&vm_name),
            "a deleted frontend model must leave the published snapshot"
        );
    }

    /// The privilege-escalation trap this design explicitly calls out: a
    /// model and a frontend model must never be allowed to share a name,
    /// because the ambiguity would make `authorize`/`resolve_target_model`'s
    /// decision (see that function's doc comment) apply to the wrong one —
    /// whichever table happens to win a lookup, silently.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_provider_model_and_a_frontend_model_may_share_a_name() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("provider_models", "name", "vm-collision-")
            .track_prefix("frontend_models", "name", "vm-collision-");
        let name = unique_name("vm-collision");

        let (status, _) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);

        let (status, _) = post_frontend_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewFrontendModel {
                name: name.clone(),
                description: String::new(),
            }),
        )
        .await
        .expect("a frontend model may take the name of the provider model it fronts");
        assert_eq!(status, StatusCode::CREATED);

        // And the reverse direction.
        let other_name = unique_name("vm-collision-reverse");
        let (status, _) = post_frontend_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewFrontendModel {
                name: other_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);

        let (status, _) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: other_name,
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .expect("and the reverse: a provider model may be created under a frontend model's name");
        assert_eq!(status, StatusCode::CREATED);
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
        let _cleanup = TestCleanup::new().track_prefix("provider_models", "name", "rebuild-test-");

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
        sqlx::query("INSERT INTO provider_models (name) VALUES ($1)")
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

        let health = admin_health(State(ctx.clone()), RequireRead).await;
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

    /// The bucket ladder, which decides how many points a chart gets.
    ///
    /// The property that matters is the cap: whatever a caller asks for,
    /// the series must stay small enough to draw and to send. A one-second
    /// bucket over a month is 2.6 million points, and the failure mode of
    /// getting this wrong is not an error — it is a response large enough to
    /// hang the browser that asked for it.
    #[test]
    fn a_bucket_width_never_yields_more_points_than_a_chart_can_draw() {
        const MAX_POINTS: i64 = 720;
        // Spans from a minute to a year, against both "no preference" and an
        // absurdly fine request that must be widened rather than honoured.
        for span in [60, 3_600, 86_400, 7 * 86_400, 30 * 86_400, 365 * 86_400] {
            for requested in [None, Some(1), Some(10)] {
                let bucket = bucket_seconds(requested, span);
                assert!(bucket > 0, "span {span}: bucket must be positive");
                let points = span / bucket;
                assert!(
                    points <= MAX_POINTS,
                    "span {span}s with requested {requested:?} gave {points} points \
                     at a {bucket}s bucket, over the {MAX_POINTS} cap"
                );
            }
        }
    }

    /// A caller asking for a *coarser* bucket than the cap requires gets it.
    /// The clamp is a floor, not an override — someone asking for hourly
    /// buckets over a day wants 24 points, not 720.
    #[test]
    fn a_coarse_bucket_request_is_honoured() {
        assert_eq!(bucket_seconds(Some(3_600), 86_400), 3_600);
        assert_eq!(bucket_seconds(Some(86_400), 30 * 86_400), 86_400);
    }

    /// A span shorter than the smallest rung still produces one usable
    /// bucket rather than a division by zero or an empty series.
    #[test]
    fn a_tiny_span_still_produces_a_bucket() {
        assert!(bucket_seconds(None, 1) >= 1);
        assert!(bucket_seconds(None, 0) >= 1);
    }

    fn usage_event(principal_id: i64, model: &str) -> UsageEvent {
        UsageEvent {
            principal_id: principal_id as u64,
            model: model.to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            usage_reported: true,
            refusal: None,
            at: chrono::Utc::now(),
            duration_ms: None,
            ttft_ms: None,
            status: None,
            requested_model: None,
            cost_micros: None,
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
            .track_prefix("provider_models", "name", "usage-basic-model-");
        let principal_name = unique_name("usage-basic-principal");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
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
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
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
    /// The point of the latency columns: metrics answer "did p99 move", this
    /// answers "for whom". Nullable throughout, so a row that genuinely has no
    /// number carries none rather than a zero indistinguishable from an
    /// instant response.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_usage_row_carries_the_latency_and_outcome_of_its_request() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "usage-latency-")
            .track_prefix("provider_models", "name", "usage-latency-model-");
        let principal_name = unique_name("usage-latency");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewPrincipal {
                name: principal_name.clone(),
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        let principal_id = created.0["id"].as_i64().unwrap();

        let model_name = unique_name("usage-latency-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();

        let mut timed = usage_event(principal_id, &model_name);
        timed.duration_ms = Some(1234);
        timed.ttft_ms = Some(56);
        timed.status = Some(200);
        timed.requested_model = Some("auto".into());
        // A second row with nothing to say, so the nullability is exercised
        // rather than assumed.
        let bare = usage_event(principal_id, &model_name);

        let _ = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![timed, bare],
            }),
        )
        .await
        .unwrap();

        // `(duration_ms, ttft_ms, status, requested_model)`.
        type Timing = (Option<i32>, Option<i32>, Option<i16>, Option<String>);
        let rows: Vec<Timing> = sqlx::query_as(
            "SELECT duration_ms, ttft_ms, status, requested_model FROM usage_events
             WHERE principal_id = $1",
        )
        .bind(principal_id)
        .fetch_all(&ctx.pool)
        .await
        .unwrap();

        // Not `ORDER BY id`: one `INSERT ... SELECT` does not promise to write
        // rows in the order the arrays were unnested, and asserting on it makes
        // a test that passes for a reason the code does not guarantee.
        assert_eq!(rows.len(), 2);
        assert!(
            rows.contains(&(Some(1234), Some(56), Some(200), Some("auto".to_string()))),
            "the timed event must arrive intact: {rows:?}"
        );
        assert!(
            rows.contains(&(None, None, None, None)),
            "an event with no timing must store nulls, not zeroes: {rows:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_batch_with_an_unknown_principal_survives_and_only_that_row_is_dropped() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "usage-principal-survives-")
            .track_prefix("provider_models", "name", "usage-model-survives-");
        let principal_name = unique_name("usage-principal-survives");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
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
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
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
            RequireConfigWrite,
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

        let listed = list_limits(State(ctx.clone()), RequireRead).await.unwrap();
        assert!(listed
            .0
            .iter()
            .any(|l| l.principal_id == principal_id && l.requests_per_min == Some(7)));

        delete_limits(State(ctx.clone()), RequireConfigWrite, Path(principal_id))
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
            RequireConfigWrite,
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
        let err = delete_limits(State(ctx.clone()), RequireConfigWrite, Path(principal_id))
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
            RequireConfigWrite,
            Path(principal_id),
            Json(PutBudget {
                tokens_total: Some(500),
                cost_total_micros: None,
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
                cost_total_micros: None,
                cost_used_micros: 0,
                tokens_total: Some(500),
                tokens_used: 0,
            })
        );

        let listed = list_budgets(State(ctx.clone()), RequireRead).await.unwrap();
        assert!(listed
            .0
            .iter()
            .any(|b| b.principal_id == principal_id && b.tokens_total == Some(500)));

        // A cost-only budget has a NULL token cap. Decoding that column as a
        // non-null `i64` made this route fail outright — the whole listing,
        // not just the offending row — the moment anyone created one.
        put_budget(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(principal_id),
            Json(PutBudget {
                tokens_total: None,
                cost_total_micros: Some(5_000_000),
                window: "monthly".into(),
            }),
        )
        .await
        .unwrap();
        let listed = list_budgets(State(ctx.clone()), RequireRead)
            .await
            .expect("a cost-only budget must not break the listing");
        assert!(listed.0.iter().any(|b| b.principal_id == principal_id
            && b.tokens_total.is_none()
            && b.cost_total_micros == Some(5_000_000)));

        delete_budget(State(ctx.clone()), RequireConfigWrite, Path(principal_id))
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
            RequireConfigWrite,
            Path(principal_id),
            Json(PutBudget {
                tokens_total: Some(100),
                cost_total_micros: None,
                window: "fortnightly".into(),
            }),
        )
        .await
        .expect_err("an unrecognised window must be rejected");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let err = put_budget(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(principal_id),
            Json(PutBudget {
                tokens_total: Some(0),
                cost_total_micros: None,
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
        let err = delete_budget(State(ctx.clone()), RequireConfigWrite, Path(principal_id))
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
            .track_prefix("provider_models", "name", "budget-usage-model-");
        let principal_id = make_principal(&ctx, &unique_name("budget-usage-principal")).await;
        let model_name = unique_name("budget-usage-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        put_budget(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(principal_id),
            Json(PutBudget {
                tokens_total: Some(1000),
                cost_total_micros: None,
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
    /// The provider's figure wins over the configured price, and the fallback
    /// rounds rather than truncating — a small request often costs single-digit
    /// micro-units, and truncating every one undercounts systematically.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_reported_cost_wins_over_the_configured_price() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "cost-src-")
            .track_prefix("provider_models", "name", "cost-src-model-");
        let principal_name = unique_name("cost-src");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewPrincipal {
                name: principal_name,
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        let principal_id = created.0["id"].as_i64().unwrap();

        let model_name = unique_name("cost-src-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                // $1 per Mtok in and out, so 3 tokens would price at 3 micros
                // exactly and 1 token at 1.
                input_price_per_mtok: Some(1_000_000),
                output_price_per_mtok: Some(1_000_000),
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();

        let mut reported = usage_event(principal_id, &model_name);
        reported.prompt_tokens = 1000;
        reported.completion_tokens = 1000;
        reported.cost_micros = Some(42);

        // Same shape, no reported cost: priced from the model instead.
        let mut priced = usage_event(principal_id, &model_name);
        priced.prompt_tokens = 1000;
        priced.completion_tokens = 1000;

        // Half a micro-unit of work: truncation would record 0, rounding 1.
        let mut tiny = usage_event(principal_id, &model_name);
        tiny.prompt_tokens = 0;
        tiny.completion_tokens = 1;

        let _ = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![reported, priced, tiny],
            }),
        )
        .await
        .unwrap();

        let costs: Vec<Option<i64>> =
            sqlx::query_scalar("SELECT cost_micros FROM usage_events WHERE principal_id = $1")
                .bind(principal_id)
                .fetch_all(&ctx.pool)
                .await
                .unwrap();
        assert!(
            costs.contains(&Some(42)),
            "the provider's own figure must win: {costs:?}"
        );
        assert!(
            costs.contains(&Some(2000)),
            "and the configured price is the fallback: {costs:?}"
        );
        assert!(
            costs.contains(&Some(1)),
            "one token at 1/Mtok rounds to 1, not down to 0: {costs:?}"
        );
    }

    /// The GUI's Audit screen. Written by a layer, and this is how it is read
    /// back without reaching for psql.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn the_audit_log_can_be_read_back_and_filtered() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new().track_prefix("provider_models", "name", "audit-read-");
        let name = unique_name("audit-read");

        sqlx::query(
            "INSERT INTO audit_events (actor_id, actor_name, action, target, detail)
             VALUES (NULL, $1, 'POST', '/admin/keys', '{\"status\":201}'::jsonb),
                    (NULL, $1, 'DELETE', '/admin/provider-models/1', '{}'::jsonb)",
        )
        .bind(&name)
        .execute(&ctx.pool)
        .await
        .unwrap();

        let all = list_audit(
            State(ctx.clone()),
            RequireRead,
            axum::extract::Query(serde_json::from_value(serde_json::json!({})).unwrap()),
        )
        .await
        .unwrap();
        // Newest first, so a UI's first page is the interesting one.
        let mine: Vec<_> = all.0.iter().filter(|a| a.actor_name == name).collect();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].action, "DELETE", "newest first");

        // Substring, so /admin/keys finds everything under it.
        let filtered = list_audit(
            State(ctx.clone()),
            RequireRead,
            axum::extract::Query(
                serde_json::from_value(serde_json::json!({"target": "/admin/keys"})).unwrap(),
            ),
        )
        .await
        .unwrap();
        assert!(filtered
            .0
            .iter()
            .filter(|a| a.actor_name == name)
            .all(|a| a.target.contains("/admin/keys")));

        // An absurd limit is clamped rather than refused: a UI asking for
        // "lots" should not read a year of history into memory.
        let clamped = list_audit(
            State(ctx.clone()),
            RequireRead,
            axum::extract::Query(
                serde_json::from_value(serde_json::json!({"limit": 100000})).unwrap(),
            ),
        )
        .await
        .unwrap();
        assert!(clamped.0.len() <= 1000);
    }

    /// The permission matrix has to be able to *change* something.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_role_can_be_granted_and_revoked_including_one_model() {
        let (ctx, _cache) = test_ctx().await;
        let role = unique_name("grant-role");
        let model = unique_name("grant-model");
        let _cleanup = TestCleanup::new()
            .track_prefix("roles", "name", "grant-role-")
            .track_prefix("provider_models", "name", "grant-model-")
            .track_prefix("permissions", "resource", "model/grant-model-");
        sqlx::query("INSERT INTO roles (name) VALUES ($1)")
            .bind(&role)
            .execute(&ctx.pool)
            .await
            .unwrap();

        let grant = |verb: &str, resource: Option<&str>| {
            let ctx = ctx.clone();
            let role = role.clone();
            let body = GrantPermission {
                verb: verb.to_string(),
                resource: resource.map(str::to_string),
            };
            async move { grant_permission(State(ctx), RequireConfigWrite, Path(role), Json(body)).await }
        };

        // An admin verb needs no resource — a matrix cell should not have to
        // know that.
        grant("usage:read", None).await.unwrap();
        // Granting twice is the state the operator sees, not a 409.
        grant("usage:read", None).await.unwrap();
        grant("model:invoke", Some(&format!("model/{model}")))
            .await
            .unwrap();

        let roles = list_roles(State(ctx.clone()), RequireRead).await.unwrap();
        let view = roles.0.iter().find(|r| r.name == role).expect("role");
        assert!(view.permissions.iter().any(|p| p.verb == "usage:read"));
        assert!(view
            .permissions
            .iter()
            .any(|p| p.verb == "model:invoke" && p.resource == format!("model/{model}")));

        // A verb nothing enforces would read as access granted on a matrix.
        assert_eq!(
            grant("model:delete", None).await.unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        // As would model:invoke on a resource that matches no model.
        assert_eq!(
            grant("model:invoke", Some("everything"))
                .await
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );

        revoke_permission(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(role.clone()),
            Json(GrantPermission {
                verb: "usage:read".into(),
                resource: None,
            }),
        )
        .await
        .unwrap();
        let roles = list_roles(State(ctx.clone()), RequireRead).await.unwrap();
        let view = roles.0.iter().find(|r| r.name == role).expect("role");
        assert!(!view.permissions.iter().any(|p| p.verb == "usage:read"));
    }

    /// The number the GUI's spend figure depends on: unpriced traffic must be
    /// counted, not silently summed as zero.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn usage_aggregates_report_unpriced_requests_separately() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "agg-")
            .track_prefix("provider_models", "name", "agg-model-");
        let principal_name = unique_name("agg");
        let (_, created) = post_principal(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewPrincipal {
                name: principal_name,
                kind: None,
                email: None,
            }),
        )
        .await
        .unwrap();
        let principal_id = created.0["id"].as_i64().unwrap();

        // One priced model and one unpriced.
        let priced = unique_name("agg-model-priced");
        let unpriced = unique_name("agg-model-unpriced");
        for (name, price) in [(&priced, Some(1_000_000)), (&unpriced, None)] {
            let _ = post_model(
                State(ctx.clone()),
                RequireConfigWrite,
                Json(NewModel {
                    policy: None,
                    name: name.clone(),
                    description: String::new(),
                    input_price_per_mtok: price,
                    output_price_per_mtok: price,
                    cache_ttl_seconds: None,
                    context_length: None,
                }),
            )
            .await
            .unwrap();
        }

        let mut a = usage_event(principal_id, &priced);
        a.prompt_tokens = 1000;
        a.completion_tokens = 0;
        let b = usage_event(principal_id, &unpriced);
        let _ = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest { events: vec![a, b] }),
        )
        .await
        .unwrap();

        let rows = usage_summary(
            State(ctx.clone()),
            RequireRead,
            axum::extract::Query(
                serde_json::from_value(serde_json::json!({"group_by": "model"})).unwrap(),
            ),
        )
        .await
        .unwrap();

        let p = rows
            .0
            .iter()
            .find(|r| r.key.as_deref() == Some(priced.as_str()))
            .expect("priced model");
        assert_eq!(p.cost_micros, 1000);
        assert_eq!(p.unpriced_requests, 0);

        let u = rows
            .0
            .iter()
            .find(|r| r.key.as_deref() == Some(unpriced.as_str()))
            .expect("unpriced model");
        assert_eq!(u.requests, 1);
        assert_eq!(
            u.unpriced_requests, 1,
            "summing a NULL cost as zero would read as 'this was cheap' when it \
             means 'this is unknown'"
        );

        assert_eq!(
            usage_summary(
                State(ctx.clone()),
                RequireRead,
                axum::extract::Query(
                    serde_json::from_value(serde_json::json!({"group_by": "'; DROP TABLE"}))
                        .unwrap()
                ),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::BAD_REQUEST,
            "group_by is an allow-list, not interpolation"
        );
    }

    /// Prices change. Before this, correcting one meant deleting the model —
    /// cascading its backends and their encrypted credentials — and recreating
    /// the lot.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_models_price_can_be_corrected_without_recreating_it() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new().track_prefix("provider_models", "name", "patch-model-");
        let name = unique_name("patch-model");
        let (_, created) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: name.clone(),
                description: "before".into(),
                input_price_per_mtok: Some(3_000_000),
                output_price_per_mtok: Some(15_000_000),
                cache_ttl_seconds: Some(60),
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let id = created.0["id"].as_i64().unwrap();

        // The read path must show them, or a price can be set and never seen.
        let listed = list_models(State(ctx.clone()), RequireRead).await.unwrap();
        let view = listed.0.iter().find(|m| m.id == id).expect("listed");
        assert_eq!(view.input_price_per_mtok, Some(3_000_000));
        assert_eq!(view.cache_ttl_seconds, Some(60));

        // Change one field. Everything omitted must be left alone — a PATCH
        // that sets a price must not silently turn caching off.
        patch_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(id),
            Json(
                serde_json::from_value(serde_json::json!({
                    "input_price_per_mtok": 4_000_000
                }))
                .unwrap(),
            ),
        )
        .await
        .unwrap();

        let listed = list_models(State(ctx.clone()), RequireRead).await.unwrap();
        let view = listed.0.iter().find(|m| m.id == id).expect("listed");
        assert_eq!(view.input_price_per_mtok, Some(4_000_000));
        assert_eq!(view.output_price_per_mtok, Some(15_000_000), "untouched");
        assert_eq!(view.cache_ttl_seconds, Some(60), "untouched");
        assert_eq!(view.description, "before", "untouched");

        // Explicit null clears, which is how a model becomes unpriced again.
        patch_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(id),
            Json(
                serde_json::from_value(serde_json::json!({
                    "input_price_per_mtok": null,
                    "cache_ttl_seconds": null
                }))
                .unwrap(),
            ),
        )
        .await
        .unwrap();

        let listed = list_models(State(ctx.clone()), RequireRead).await.unwrap();
        let view = listed.0.iter().find(|m| m.id == id).expect("listed");
        assert_eq!(view.input_price_per_mtok, None, "null clears");
        assert_eq!(view.cache_ttl_seconds, None);
        assert_eq!(
            view.output_price_per_mtok,
            Some(15_000_000),
            "still untouched"
        );

        let missing = patch_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(-1),
            Json(serde_json::from_value(serde_json::json!({"description": "x"})).unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_service_account_credential_is_validated_when_the_backend_is_created() {
        let (ctx, _cache) = test_ctx().await;
        // Backends cascade from the model, so tracking the model is enough.
        let _cleanup = TestCleanup::new().track_prefix("provider_models", "name", "gcp-validate-");
        let model_name = unique_name("gcp-validate");
        let (_, model) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let provider_model_id = model.0["id"].as_i64().unwrap();

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
            RequireConfigWrite,
            Path(provider_model_id),
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
            RequireConfigWrite,
            Path(provider_model_id),
            Json(vertex(None, Some("gcp_service_account"))),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        // An unknown kind names the valid ones rather than reaching the
        // column's CHECK constraint as a Postgres error.
        let err = post_backend(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(provider_model_id),
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
            RequireConfigWrite,
            Path(provider_model_id),
            Json(vertex(Some(key_file), Some("gcp_service_account"))),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created.0["credential_kind"], "gcp_service_account");

        // And the default stays `static`, so every existing caller is
        // unaffected by the field existing. On its own model, because a
        // provider model has exactly one provider and the row above already
        // took this one's.
        let (_, plain_model) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: unique_name("gcp-validate"),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let (_, plain) = post_backend(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(plain_model.0["id"].as_i64().unwrap()),
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
            .track_prefix("provider_models", "name", "no-budget-usage-model-");
        let principal_id = make_principal(&ctx, &unique_name("no-budget-usage-principal")).await;
        let model_name = unique_name("no-budget-usage-model");
        let _ = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
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

    /// The question a rule author has: *which* rule fired, not merely where
    /// the request ended up. A dry run that returned only a model name would
    /// leave "my second rule matched instead of my first" indistinguishable
    /// from "my first rule matched and points somewhere I did not expect".
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_dry_run_names_the_rule_that_decided_and_falls_back_to_the_defaults() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("provider_models", "name", "dry-fast-")
            .track_prefix("provider_models", "name", "dry-slow-")
            .track_prefix("frontend_models", "name", "dry-vm-");

        let model = |name: String| {
            let ctx = ctx.clone();
            async move {
                post_model(
                    State(ctx),
                    RequireConfigWrite,
                    Json(NewModel {
                        policy: None,
                        name,
                        description: String::new(),
                        input_price_per_mtok: None,
                        output_price_per_mtok: None,
                        cache_ttl_seconds: None,
                        context_length: None,
                    }),
                )
                .await
                .unwrap()
                .1
                 .0["id"]
                    .as_i64()
                    .unwrap()
            }
        };
        let fast_name = unique_name("dry-fast");
        let slow_name = unique_name("dry-slow");
        let fast_id = model(fast_name.clone()).await;
        let slow_id = model(slow_name.clone()).await;

        let vm_name = unique_name("dry-vm");
        let (_, vm) = post_frontend_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewFrontendModel {
                name: vm_name.clone(),
                description: String::new(),
            }),
        )
        .await
        .unwrap();
        let vm_id = vm.0["id"].as_i64().unwrap();

        // One rule, on streaming, so the dry run can be flipped either side of
        // it without touching the database again.
        let (_, rule) = post_rule(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(vm_id),
            Json(NewRule {
                position: 0,
                match_condition: MatchConditionJson {
                    stream: Some(true),
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap();
        let rule_id = rule.0["id"].as_i64().unwrap();
        let _ = post_rule_target(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(rule_id),
            Json(NewTarget {
                provider_model_id: fast_id,
                weight: 100,
                position: 0,
            }),
        )
        .await
        .unwrap();
        let _ = post_default_target(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(vm_id),
            Json(NewTarget {
                provider_model_id: slow_id,
                weight: 100,
                position: 0,
            }),
        )
        .await
        .unwrap();

        let dry_run = |streaming: bool, model: String| {
            let ctx = ctx.clone();
            async move {
                routing_dry_run(
                    State(ctx),
                    RequireRead,
                    Json(DryRunRequest {
                        model,
                        principal_id: None,
                        streaming,
                        prompt_tokens: 0,
                        max_tokens: None,
                        headers: Default::default(),
                        class: None,
                        class_refines: Vec::new(),
                    }),
                )
                .await
                .unwrap()
                .0
            }
        };

        let matched = dry_run(true, vm_name.clone()).await;
        assert_eq!(matched.candidates.first().unwrap(), &fast_name);
        assert_eq!(
            matched.matched_rule,
            Some(0),
            "the rule index is the answer to \"why did it go there\""
        );

        let defaulted = dry_run(false, vm_name.clone()).await;
        assert_eq!(defaulted.candidates.first().unwrap(), &slow_name);
        assert_eq!(
            defaulted.matched_rule, None,
            "no rule matched; the defaults decided"
        );

        // Any name a UI can type must be answerable, including a concrete
        // model, which resolves to itself rather than erroring.
        let concrete = dry_run(true, fast_name.clone()).await;
        assert!(!concrete.frontend_model);
        assert_eq!(concrete.candidates, vec![fast_name]);
    }

    /// The audit log's value is that every row is a change. A dry run is a
    /// read that happens to need a body, and recording it dilutes the log the
    /// same way auditing `GET` would.
    #[test]
    fn a_post_that_changes_nothing_is_not_recorded_as_a_change() {
        assert!(is_read_only("/admin/routing/dry-run"));
        assert!(is_read_only("/admin/prompt-classes/evaluate"));
        // Everything that does change something must stay audited, including
        // the routes added alongside the exceptions.
        for path in [
            "/admin/keys",
            "/admin/prices/sync",
            "/admin/roles/operator/permissions",
            "/admin/provider-models/1",
        ] {
            assert!(!is_read_only(path), "{path} must stay audited");
        }
    }

    /// A role exists so permissions have somewhere to live; without one, "this
    /// app may call these two models and nothing else" is unexpressible and
    /// the only option is the seeded `inference` role, which grants every
    /// model including the paid ones.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_scoped_role_can_be_created_and_is_not_deletable_while_held() {
        let (ctx, _cache) = test_ctx().await;
        let role_name = unique_name("scoped-role");
        let _cleanup = TestCleanup::new()
            .track_prefix("roles", "name", "scoped-role-")
            .track_prefix("principals", "name", "scoped-holder-");

        let (status, created) = post_role(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewRole {
                name: role_name.clone(),
                description: Some("only the local models".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        assert!(created.0["id"].is_i64());

        // The same name twice is a conflict, not a second row.
        assert_eq!(
            post_role(
                State(ctx.clone()),
                RequireConfigWrite,
                Json(NewRole {
                    name: role_name.clone(),
                    description: None,
                }),
            )
            .await
            .expect_err("a duplicate role name must be refused")
            .0,
            StatusCode::CONFLICT
        );

        // Deleting it while a principal holds it would take that principal's
        // access away all at once, with the symptom arriving much later.
        let principal_id = make_principal(&ctx, &unique_name("scoped-holder")).await;
        grant_role(
            State(ctx.clone()),
            RequireConfigWrite,
            Path(principal_id),
            Json(RoleGrant {
                role: role_name.clone(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            delete_role(
                State(ctx.clone()),
                RequireConfigWrite,
                Path(role_name.clone()),
            )
            .await
            .expect_err("a held role must not be deletable")
            .0,
            StatusCode::CONFLICT
        );

        // Once nobody holds it, it goes.
        revoke_role(
            State(ctx.clone()),
            RequireConfigWrite,
            Path((principal_id, role_name.clone())),
        )
        .await
        .unwrap();
        assert_eq!(
            delete_role(
                State(ctx.clone()),
                RequireConfigWrite,
                Path(role_name.clone()),
            )
            .await
            .unwrap(),
            StatusCode::NO_CONTENT
        );
    }

    /// The blanket `model:invoke` grant is stored as `model/*`, not `*` — the
    /// resource is namespaced and `validate_grant` refuses a bare `*` for this
    /// verb. Pinned because a reader that checked for `*` showed a role with
    /// access to every model as having access to none, and nothing failed.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_blanket_model_grant_is_namespaced_and_a_bare_star_is_refused() {
        let (ctx, _cache) = test_ctx().await;
        let roles = list_roles(State(ctx.clone()), RequireRead).await.unwrap().0;
        let inference = roles
            .iter()
            .find(|r| r.name == "inference")
            .expect("the seeded inference role");
        assert!(
            inference
                .permissions
                .iter()
                .any(|p| p.verb == "model:invoke" && p.resource == "model/*"),
            "the wildcard grant is `model/*`; anything reading it as `*` sees no grant at all"
        );
        assert!(validate_grant("model:invoke", "*").is_err());
        assert!(validate_grant("model:invoke", "model/*").is_ok());
        assert!(validate_grant("model:invoke", "model/gpt-4o").is_ok());
        // The reverse: an admin verb is only meaningful unscoped.
        assert!(validate_grant("config:write", "model/x").is_err());
        assert!(validate_grant("config:write", "*").is_ok());
    }

    /// A settings screen that guesses defaults is worse than one showing
    /// nothing: this must report the flags the process actually has.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn the_config_route_reports_this_processs_own_flags() {
        let (ctx, _cache) = test_ctx().await;
        let cfg = get_config(State(ctx.clone()), RequireRead, AdminPrincipal(1))
            .await
            .unwrap()
            .0;
        assert_eq!(cfg["role"], "all");
        assert_eq!(cfg["config_poll_seconds"], 5);
        assert_eq!(cfg["cache_max_entries"], 4096);
        assert_eq!(cfg["session_ttl_hours"], 12);
        // A build without the feature must say so rather than omit the field
        // and leave a UI to guess.
        assert_eq!(cfg["otel_endpoint"], serde_json::Value::Null);
        assert!(cfg["models"].is_i64());
    }

    /// The manual rebuild has to say which snapshot it published: `refresh`
    /// deliberately does not fail the request that triggered it, so a bare
    /// 200 would not distinguish "rebuilt" from "tried and failed".
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_manual_rebuild_reports_the_version_it_published() {
        let (ctx, cache) = test_ctx().await;
        let resp = rebuild_snapshot(State(ctx.clone()), RequireConfigWrite)
            .await
            .unwrap()
            .0;
        assert_eq!(
            resp["snapshot_version"].as_u64().unwrap(),
            cache.current_snapshot().version,
            "the version reported must be the one actually published"
        );
        assert_eq!(resp["rebuild_failures"], 0);
    }

    /// Grouping by the *served* model cannot answer "how much traffic does
    /// each frontend model carry" — by then the routing decision is made and
    /// the virtual name is gone. This is the only grouping that keeps it.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn usage_can_be_grouped_by_the_virtual_model_the_caller_asked_for() {
        let (ctx, _cache) = test_ctx().await;
        let _cleanup = TestCleanup::new()
            .track_prefix("principals", "name", "vgroup-p-")
            .track_prefix("provider_models", "name", "vgroup-m-");
        let principal_id = make_principal(&ctx, &unique_name("vgroup-p")).await;
        let model_name = unique_name("vgroup-m");
        let (_, model) = post_model(
            State(ctx.clone()),
            RequireConfigWrite,
            Json(NewModel {
                policy: None,
                name: model_name.clone(),
                description: String::new(),
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_ttl_seconds: None,
                context_length: None,
            }),
        )
        .await
        .unwrap();
        let _ = model;

        // One request that asked for a virtual name, one that asked for the
        // model directly — they must not collapse into one row.
        let mut asked_for_virtual = usage_event(principal_id, &model_name);
        asked_for_virtual.requested_model = Some("vgroup-router".into());
        let resp = post_usage(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(UsageBatchRequest {
                events: vec![asked_for_virtual, usage_event(principal_id, &model_name)],
            }),
        )
        .await
        .unwrap();
        assert_eq!(resp.0.accepted, 2);

        let rows = usage_summary(
            State(ctx.clone()),
            RequireRead,
            axum::extract::Query(UsageQuery {
                group_by: "frontend_model".into(),
                since: None,
                until: None,
                limit: 1000,
            }),
        )
        .await
        .unwrap()
        .0;
        let router = rows
            .iter()
            .find(|r| r.key.as_deref() == Some("vgroup-router"))
            .expect("the virtual name the caller asked for must be its own row");
        assert_eq!(router.requests, 1);
        assert!(
            rows.iter().any(|r| r.key.as_deref() == Some(&model_name)),
            "a request that named a provider model still groups under it"
        );
        // Deliberately checked here rather than assumed: an unknown grouping
        // must be refused, not interpolated into the query text.
        assert!(usage_summary(
            State(ctx.clone()),
            RequireRead,
            axum::extract::Query(UsageQuery {
                group_by: "; DROP TABLE provider_models".into(),
                since: None,
                until: None,
                limit: 10,
            }),
        )
        .await
        .is_err());
    }

    /// The reverse channel end to end: a proxy's report survives the route and
    /// comes back out of `GET /admin/fleet`, on the proxy token and not on a
    /// human's session.
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn a_health_report_is_gated_by_the_proxy_token_and_read_back_from_the_fleet() {
        let (ctx, _cache) = test_ctx().await;
        let report = crate::health_report::HealthReport {
            replica: "proxy-test-0".into(),
            snapshot_version: 42,
            uptime_seconds: 7,
            process: crate::health_report::ProcessCounters {
                cache_hits: 9,
                cache_misses: 1,
                usage_dropped: 3,
                ..Default::default()
            },
            backends: vec![crate::health_report::BackendHealth {
                api_base: "http://backend:8000".into(),
                model: "m".into(),
                healthy: false,
                inflight: 3,
                requests_total: 100,
                errors_total: 9,
            }],
        };

        let rejected = post_health_report(
            State(ctx.clone()),
            auth_header("not-the-proxy-token"),
            Json(report.clone()),
        )
        .await
        .into_response();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        assert!(
            list_fleet(State(ctx.clone()), RequireRead)
                .await
                .0
                .is_empty(),
            "a rejected report must not be recorded"
        );

        let accepted = post_health_report(
            State(ctx.clone()),
            auth_header(&ctx.proxy_token),
            Json(report),
        )
        .await
        .into_response();
        assert!(accepted.status().is_success());

        let fleet = list_fleet(State(ctx.clone()), RequireRead).await.0;
        assert_eq!(fleet.len(), 1);
        assert_eq!(fleet[0].snapshot_version, 42);
        assert!(
            !fleet[0].backends[0].healthy,
            "an unhealthy backend must stay unhealthy across the channel; \
             this is the whole reason the report exists"
        );
        assert_eq!(fleet[0].backends[0].inflight, 3);
        // Per-process counters travel with the report: a fleet view can only
        // show the spread in cache hit rate if it has each replica's own.
        assert_eq!(fleet[0].process.cache_hits, 9);
        assert_eq!(fleet[0].process.usage_dropped, 3);
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
        sqlx::query_as("SELECT id, name FROM provider_models WHERE is_fallback LIMIT 1")
            .fetch_optional(&ctx.pool)
            .await
            .map_err(|e| db_error("reading the fallback model", &e))?;
    Ok(Json(match row {
        Some((id, name)) => serde_json::json!({"id": id, "name": name}),
        None => serde_json::json!({"id": null, "name": null}),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackModel {
    /// `null` clears it, leaving no deployment-wide last resort.
    provider_model_id: Option<i64>,
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
    sqlx::query("UPDATE provider_models SET is_fallback = false WHERE is_fallback")
        .execute(&mut *tx)
        .await
        .map_err(|e| db_error("clearing the previous fallback model", &e))?;

    let mut name = None;
    if let Some(id) = body.provider_model_id {
        let updated: Option<String> = sqlx::query_scalar(
            "UPDATE provider_models SET is_fallback = true WHERE id = $1 RETURNING name",
        )
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
        serde_json::json!({"provider_model_id": body.provider_model_id, "name": name}),
    ))
}
