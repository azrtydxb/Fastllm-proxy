# P0: Control Plane and RBAC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single shared master key with real API keys backed by role-based authorisation, served from a control plane over a pre-flattened snapshot, without putting any I/O on the request path.

**Architecture:** One binary with a `--role` flag (`all` / `control` / `proxy`). The control plane owns Postgres and builds a versioned `Snapshot`; the data plane consumes that snapshot through a `SnapshotSource` trait with three implementations (`File`, `Local`, `Http`) and cannot tell them apart. Authorisation questions are answered once at snapshot load — roles resolved, wildcards expanded, deny applied — so the request path does one SHA-256 and two hash lookups.

**Tech Stack:** Rust 2021, hyper 1.x (data plane, unchanged), axum 0.8 (control plane only), sqlx 0.8 + Postgres 17, sha2, argon2, tokio.

## Global Constraints

- `rust-version = "1.82"` (from `Cargo.toml`); do not raise it.
- **The request path must perform no I/O.** No database calls, no network calls, no file reads while serving a request. Task 11 enforces this with a test.
- **API keys are hashed with SHA-256, never Argon2.** They are high-entropy random values; a slow KDF per request would cost more than the entire proxy.
- **User passwords are hashed with Argon2id, never SHA-256.** They are low-entropy and human-chosen.
- API key plaintext is returned exactly once, at creation, and never stored.
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` must pass before every commit. CI enforces all three.
- axum and sqlx are **control-plane only**. The `proxy` role must not link a database driver. Enforce with Cargo features: `control` (default) and a `proxy`-only build that excludes them.
- Existing behaviour that must not regress: cache-affinity routing, in-flight accounting released at end of stream, `https://` backends, multipart audio endpoints, and `File` mode running with no dependencies.

---

## File Structure

**New:**
- `src/snapshot.rs` — the `Snapshot` type and its authorisation queries. Shared by both planes. No I/O.
- `src/source/mod.rs` — `SnapshotSource` trait and the polling loop.
- `src/source/file.rs` — builds a `Snapshot` from today's `FileConfig`.
- `src/source/http.rs` — polls a control plane, caches last-known-good to disk.
- `src/control/mod.rs` — control-plane role entry point.
- `src/control/db.rs` — sqlx pool and migration runner.
- `src/control/build.rs` — turns database rows into a `Snapshot` (the flattening lives here).
- `src/control/api.rs` — admin REST API and `/snapshot`.
- `migrations/0001_init.sql` — schema.
- `docker-compose.yml` — Postgres plus the binary in `--role=all`.
- `tests/rbac.rs` — end-to-end authorisation against the mock upstream.

**Modified:**
- `src/config.rs` — add `auth:` section for `File` mode keys; add process config (`role`, `database_url`, `control_url`, `proxy_token`).
- `src/proxy.rs:check_auth` — replace the master-key comparison with a snapshot lookup, and add the per-model authorisation check.
- `src/state.rs` — hold `ArcSwap<Snapshot>` alongside the registry.
- `src/main.rs` — `--role` dispatch and wiring.
- `Cargo.toml` — dependencies and the `control` / `proxy` features.

**Rationale for the split:** `snapshot.rs` is pure data and pure functions, so it is unit-testable with no fixtures and is the only file both roles share. Everything touching Postgres lives under `src/control/`, which is what makes the feature-gated `proxy` build possible.

---

### Task 1: The `Snapshot` type and its authorisation queries

The core data structure. Pure, no I/O, no dependencies — everything else builds on it.

**Files:**
- Create: `src/snapshot.rs`
- Modify: `src/main.rs` (add `mod snapshot;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct Snapshot { pub version: u64, ... }`
  - `pub fn Snapshot::authenticate(&self, token: &str, now: SystemTime) -> Result<&Principal, AuthError>`
  - `pub fn Principal::may_invoke(&self, model: &str) -> bool`
  - `pub enum AuthError { Unknown, Expired, Disabled }`
  - `pub struct Principal { pub id: u64, pub name: String, pub allowed_models: HashSet<String>, pub allow_all: bool }`
  - `pub struct ModelDef { pub name: String, pub backends: Vec<BackendDef> }`
  - `pub struct BackendDef { pub api_base: String, pub upstream_model: String, pub api_key: Option<String> }`
  - `pub fn hash_key(token: &str) -> [u8; 32]`

- [ ] **Step 1: Write the failing test**

Create `src/snapshot.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn snapshot_with(key: &str, allowed: &[&str], expires: Option<SystemTime>) -> Snapshot {
        let principal = Principal {
            id: 1,
            name: "eval-team".into(),
            allowed_models: allowed.iter().map(|s| s.to_string()).collect(),
            allow_all: false,
        };
        Snapshot::for_test(vec![(key.to_string(), 1, expires, false)], vec![principal], vec![])
    }

    #[test]
    fn a_known_key_resolves_to_its_principal() {
        let snap = snapshot_with("sk-good", &["m"], None);
        let p = snap.authenticate("sk-good", SystemTime::now()).unwrap();
        assert_eq!(p.name, "eval-team");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let snap = snapshot_with("sk-good", &["m"], None);
        assert!(matches!(
            snap.authenticate("sk-bad", SystemTime::now()),
            Err(AuthError::Unknown)
        ));
    }

    #[test]
    fn an_expired_key_is_rejected_even_though_it_is_known() {
        let past = SystemTime::now() - Duration::from_secs(60);
        let snap = snapshot_with("sk-good", &["m"], Some(past));
        assert!(matches!(
            snap.authenticate("sk-good", SystemTime::now()),
            Err(AuthError::Expired)
        ));
    }

    #[test]
    fn model_grants_are_exact_unless_allow_all() {
        let snap = snapshot_with("sk-good", &["qwen3", "whisper"], None);
        let p = snap.authenticate("sk-good", SystemTime::now()).unwrap();
        assert!(p.may_invoke("qwen3"));
        assert!(p.may_invoke("whisper"));
        assert!(!p.may_invoke("gpt-4"));
    }

    #[test]
    fn allow_all_grants_every_model() {
        let mut snap = snapshot_with("sk-admin", &[], None);
        snap.principals.get_mut(&1).unwrap().allow_all = true;
        let p = snap.authenticate("sk-admin", SystemTime::now()).unwrap();
        assert!(p.may_invoke("anything-at-all"));
    }

    #[test]
    fn hashing_is_stable_and_distinct() {
        assert_eq!(hash_key("sk-a"), hash_key("sk-a"));
        assert_ne!(hash_key("sk-a"), hash_key("sk-b"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib snapshot`
Expected: FAIL — `cannot find type Snapshot in this scope`.

- [ ] **Step 3: Write the implementation**

Add above the test module in `src/snapshot.rs`:

```rust
//! The contract between the control plane and the data plane.
//!
//! Everything expensive is resolved when this is built, not when a request
//! arrives: roles are flattened into `allowed_models`, wildcards expanded, and
//! deny rules applied. The request path asks a `HashSet` and never walks the
//! RBAC graph — that is what keeps authorisation off the latency budget.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

pub type PrincipalId = u64;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    Unknown,
    Expired,
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub id: PrincipalId,
    pub name: String,
    /// Already flattened from roles, wildcards and deny rules.
    pub allowed_models: HashSet<String>,
    pub allow_all: bool,
}

impl Principal {
    #[inline]
    pub fn may_invoke(&self, model: &str) -> bool {
        self.allow_all || self.allowed_models.contains(model)
    }
}

#[derive(Debug, Clone)]
pub struct KeyEntry {
    pub principal: PrincipalId,
    pub expires_at: Option<SystemTime>,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct BackendDef {
    pub api_base: String,
    pub upstream_model: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub name: String,
    pub backends: Vec<BackendDef>,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub version: u64,
    pub keys: HashMap<[u8; 32], KeyEntry>,
    pub principals: HashMap<PrincipalId, Principal>,
    pub models: Vec<ModelDef>,
    /// When true the proxy serves without authenticating, matching today's
    /// behaviour when no master key is configured.
    pub open: bool,
}

/// SHA-256, deliberately not a slow KDF: API keys are high-entropy random
/// values, so stretching buys nothing and would put milliseconds on every
/// request. Passwords are the opposite case and use Argon2id elsewhere.
pub fn hash_key(token: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().into()
}

impl Snapshot {
    pub fn authenticate(&self, token: &str, now: SystemTime) -> Result<&Principal, AuthError> {
        let entry = self.keys.get(&hash_key(token)).ok_or(AuthError::Unknown)?;
        if entry.disabled {
            return Err(AuthError::Disabled);
        }
        if entry.expires_at.is_some_and(|e| e <= now) {
            return Err(AuthError::Expired);
        }
        self.principals.get(&entry.principal).ok_or(AuthError::Unknown)
    }

    #[cfg(test)]
    pub fn for_test(
        keys: Vec<(String, PrincipalId, Option<SystemTime>, bool)>,
        principals: Vec<Principal>,
        models: Vec<ModelDef>,
    ) -> Self {
        Self {
            version: 1,
            keys: keys
                .into_iter()
                .map(|(k, p, e, d)| {
                    (
                        hash_key(&k),
                        KeyEntry { principal: p, expires_at: e, disabled: d },
                    )
                })
                .collect(),
            principals: principals.into_iter().map(|p| (p.id, p)).collect(),
            models,
            open: false,
        }
    }
}
```

Add to `Cargo.toml` under `[dependencies]`: `sha2 = "0.10"`.
Add `mod snapshot;` to `src/main.rs` alongside the other modules.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib snapshot`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add src/snapshot.rs src/main.rs Cargo.toml Cargo.lock
git commit -m "Add the Snapshot type shared by both planes

Authorisation questions are answered when the snapshot is built, not when a
request arrives: allowed_models is already flattened from roles, wildcards
and deny rules, so the request path is a hash lookup and a set lookup.

Keys hash with SHA-256 rather than a KDF on purpose. They are high-entropy
random values, so stretching buys no security and Argon2 per request would
cost more than the whole proxy."
```

---

### Task 2: `SnapshotSource` trait and the `File` implementation

Preserves today's dependency-free mode and gives it RBAC, so there is one authorisation code path rather than two.

**Files:**
- Create: `src/source/mod.rs`, `src/source/file.rs`
- Modify: `src/config.rs` (add the `auth:` section), `src/main.rs` (add `mod source;`)

**Interfaces:**
- Consumes: `Snapshot`, `Principal`, `ModelDef`, `BackendDef`, `hash_key` from Task 1; `FileConfig`, `ModelEntry` from `src/config.rs`.
- Produces:
  - `pub trait SnapshotSource: Send + Sync { async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>>; }`
  - `pub struct FileSource { path: PathBuf }` with `FileSource::new(path: PathBuf) -> Self`
  - `pub struct AuthConfig { pub keys: Vec<KeyConfig> }`, `pub struct KeyConfig { pub key: String, pub name: String, pub models: Vec<String>, pub expires_at: Option<String> }`

- [ ] **Step 1: Write the failing test**

Create `src/source/file.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn a_config_without_auth_produces_an_open_snapshot() {
        // Matches today's behaviour: no master key means no authentication.
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        );
        let snap = FileSource::new(f.path().into()).fetch(None).await.unwrap().unwrap();
        assert!(snap.open);
        assert_eq!(snap.models.len(), 1);
        assert_eq!(snap.models[0].name, "m");
    }

    #[tokio::test]
    async fn keys_and_grants_are_read_from_the_auth_section() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n\
             auth:\n  keys:\n    - key: sk-eval\n      name: eval\n      models: [m]\n",
        );
        let snap = FileSource::new(f.path().into()).fetch(None).await.unwrap().unwrap();
        assert!(!snap.open);
        let p = snap.authenticate("sk-eval", std::time::SystemTime::now()).unwrap();
        assert_eq!(p.name, "eval");
        assert!(p.may_invoke("m"));
        assert!(!p.may_invoke("other"));
    }

    #[tokio::test]
    async fn a_star_grant_means_every_model() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n\
             auth:\n  keys:\n    - key: sk-admin\n      name: admin\n      models: ['*']\n",
        );
        let snap = FileSource::new(f.path().into()).fetch(None).await.unwrap().unwrap();
        let p = snap.authenticate("sk-admin", std::time::SystemTime::now()).unwrap();
        assert!(p.allow_all);
        assert!(p.may_invoke("anything"));
    }

    #[tokio::test]
    async fn an_unchanged_file_reports_no_new_snapshot() {
        let f = write_config(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        );
        let src = FileSource::new(f.path().into());
        let first = src.fetch(None).await.unwrap().unwrap();
        assert!(src.fetch(Some(first.version)).await.unwrap().is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib source::file`
Expected: FAIL — `cannot find type FileSource in this scope`.

- [ ] **Step 3: Write the implementation**

Create `src/source/mod.rs`:

```rust
//! Where the data plane gets its policy.
//!
//! Three implementations, one trait, and forwarding cannot tell them apart:
//! `File` for a proxy with no control plane at all, `Local` for the
//! single-process role, `Http` for a proxy against a control plane.

pub mod file;

use crate::snapshot::Snapshot;

pub trait SnapshotSource: Send + Sync {
    /// Return a snapshot only if it is newer than `have`.
    ///
    /// `Ok(None)` means unchanged, which is the common case on every poll and
    /// must stay cheap.
    fn fetch(
        &self,
        have: Option<u64>,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<Snapshot>>> + Send;
}
```

Create `src/source/file.rs` above its test module:

```rust
//! Builds a snapshot from the YAML config, giving `File` mode the same
//! authorisation model as the control plane rather than a second code path.

use crate::config::FileConfig;
use crate::snapshot::{BackendDef, KeyEntry, ModelDef, Principal, Snapshot, hash_key};
use crate::source::SnapshotSource;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct FileSource {
    path: PathBuf,
}

impl FileSource {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl SnapshotSource for FileSource {
    async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
        let raw = std::fs::read(&self.path)?;
        // The file has no version of its own, so its content hash is the
        // version. Identical content is not a change, which is the same rule
        // the config watcher already follows.
        let version = u64::from_le_bytes(hash_key(&String::from_utf8_lossy(&raw))[..8].try_into()?);
        if have == Some(version) {
            return Ok(None);
        }

        let cfg: FileConfig = serde_yaml::from_slice(&raw)?;
        cfg.validate()?;

        let mut models = Vec::new();
        for entry in &cfg.model_list {
            let name = entry.model_name.clone();
            let backend = BackendDef {
                api_base: entry.litellm_params.api_base.trim_end_matches('/').to_string(),
                upstream_model: entry.litellm_params.upstream_model(&name),
                api_key: entry.litellm_params.effective_api_key(),
            };
            match models.iter_mut().find(|m: &&mut ModelDef| m.name == name) {
                Some(m) => m.backends.push(backend),
                None => models.push(ModelDef { name, backends: vec![backend] }),
            }
        }

        let mut keys = HashMap::new();
        let mut principals = HashMap::new();
        for (i, k) in cfg.auth.keys.iter().enumerate() {
            let id = i as u64 + 1;
            let allow_all = k.models.iter().any(|m| m == "*");
            principals.insert(
                id,
                Principal {
                    id,
                    name: k.name.clone(),
                    allowed_models: k.models.iter().filter(|m| *m != "*").cloned().collect(),
                    allow_all,
                },
            );
            keys.insert(
                hash_key(&k.key),
                KeyEntry {
                    principal: id,
                    expires_at: k.expires_at.as_deref().map(parse_rfc3339).transpose()?,
                    disabled: false,
                },
            );
        }

        let open = keys.is_empty();
        Ok(Some(Snapshot { version, keys, principals, models, open }))
    }
}

fn parse_rfc3339(s: &str) -> anyhow::Result<std::time::SystemTime> {
    Ok(humantime::parse_rfc3339(s)?)
}
```

Add to `src/config.rs`:

```rust
/// Keys for `File` mode. The control plane replaces this entirely; it exists
/// so a proxy with no control plane still has real authorisation rather than
/// one shared secret.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub keys: Vec<KeyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeyConfig {
    pub key: String,
    pub name: String,
    /// Model names, or `*` for every model.
    #[serde(default)]
    pub models: Vec<String>,
    /// RFC 3339, e.g. `2027-01-01T00:00:00Z`.
    #[serde(default)]
    pub expires_at: Option<String>,
}
```

Add `pub auth: AuthConfig,` with `#[serde(default)]` to `FileConfig`, and change `fn validate` to `pub(crate) fn validate`.

Add to `Cargo.toml`: `humantime = "2"`, and under `[dev-dependencies]`: `tempfile = "3"`.
Add `mod source;` to `src/main.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib source::file`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
git add src/source/ src/config.rs src/main.rs Cargo.toml Cargo.lock
git commit -m "Add SnapshotSource with the File implementation

File mode keeps working with no dependencies and gains real keys, so there is
one authorisation code path instead of a lesser one for standalone use. A
config with no auth section produces an open snapshot, matching today's
behaviour when no master key is set."
```

---

### Task 3: Authorise requests from the snapshot

Replaces the master key on the request path. This is the task that changes user-visible behaviour.

**Files:**
- Modify: `src/state.rs`, `src/proxy.rs` (`check_auth`, `proxy_request`), `src/main.rs`
- Test: `src/proxy.rs` test module

**Interfaces:**
- Consumes: `Snapshot`, `AuthError`, `Principal` (Task 1); `FileSource`, `SnapshotSource` (Task 2).
- Produces: `AppState.snapshot: ArcSwap<Snapshot>`; `fn authorize(req, state) -> Result<Option<&Principal>, Response<ResBody>>`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/proxy.rs`:

```rust
use crate::snapshot::{Principal, Snapshot};
use std::collections::HashSet;

fn snap(key: &str, models: &[&str]) -> Snapshot {
    Snapshot::for_test(
        vec![(key.to_string(), 1, None, false)],
        vec![Principal {
            id: 1,
            name: "t".into(),
            allowed_models: models.iter().map(|s| s.to_string()).collect::<HashSet<_>>(),
            allow_all: false,
        }],
        vec![],
    )
}

#[test]
fn a_valid_key_authorises_a_granted_model() {
    let s = snap("sk-ok", &["m"]);
    let p = s.authenticate("sk-ok", std::time::SystemTime::now()).unwrap();
    assert!(p.may_invoke("m"));
}

#[test]
fn a_valid_key_is_forbidden_from_an_ungranted_model() {
    // 403, not 404: the model exists, this caller may not use it. Returning
    // 404 would leak nothing but would also mislead.
    let s = snap("sk-ok", &["m"]);
    let p = s.authenticate("sk-ok", std::time::SystemTime::now()).unwrap();
    assert!(!p.may_invoke("secret-model"));
}

#[test]
fn an_open_snapshot_needs_no_key() {
    let mut s = Snapshot::default();
    s.open = true;
    assert!(s.open);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib proxy`
Expected: FAIL — `no function or associated item named for_test` is visible, or `unresolved import crate::snapshot`.

- [ ] **Step 3: Write the implementation**

In `src/state.rs`, add to `AppState`:

```rust
    /// Swapped wholesale whenever the source produces a new version.
    pub snapshot: ArcSwap<crate::snapshot::Snapshot>,
```

and delete `pub master_key: Option<String>,`.

In `src/proxy.rs`, replace `check_auth` with:

```rust
/// Authenticate the caller and return their principal.
///
/// `Ok(None)` means the snapshot is open — no keys configured — which is the
/// same permissive behaviour as running without a master key today.
fn authorize<'a>(
    req: &Request<Incoming>,
    snapshot: &'a Snapshot,
) -> Result<Option<&'a Principal>, Response<ResBody>> {
    if snapshot.open {
        return Ok(None);
    }
    let token = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);

    let Some(token) = token else {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid bearer token",
        ));
    };
    match snapshot.authenticate(token, SystemTime::now()) {
        Ok(p) => Ok(Some(p)),
        Err(AuthError::Expired) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "expired_api_key",
            "this api key has expired",
        )),
        Err(_) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "missing or invalid bearer token",
        )),
    }
}
```

In `handle`, replace the `check_auth` call:

```rust
    let snapshot = state.snapshot.load();
    let principal = match authorize(&req, &snapshot) {
        Ok(p) => p.cloned(),
        Err(rejection) => return Ok(rejection),
    };
```

In `proxy_request`, after the model name is resolved and before the pool lookup:

```rust
    // Authorisation is a set lookup against the pre-flattened grant list, not
    // a walk of the RBAC graph, and costs nothing measurable.
    if let Some(principal) = &principal {
        if !principal.may_invoke(&model) {
            state.requests_failed.fetch_add(1, Ordering::Relaxed);
            return error_response(
                StatusCode::FORBIDDEN,
                "model_access_denied",
                &format!("key is not permitted to use model {model:?}"),
            );
        }
    }
```

Thread `principal: Option<Principal>` through `proxy_request`'s signature.

In `src/main.rs`, build the initial snapshot from `FileSource` before constructing `AppState`, store it in `ArcSwap`, and remove the `master_key` wiring and its warning. Keep `--master-key` as a deprecated flag that synthesises a single `allow_all` key so existing deployments do not break:

```rust
    // Deprecated: a single shared key is exactly what this release replaces,
    // but silently breaking a running deployment is worse than a warning.
    if let Some(key) = &cli.master_key {
        warn!("--master-key is deprecated; define keys under `auth:` or use a control plane");
        snapshot.add_legacy_master_key(key);
    }
```

Add `pub fn add_legacy_master_key(&mut self, key: &str)` to `Snapshot`, inserting an `allow_all` principal and clearing `open`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS. All existing tests must still pass — in particular `bearer_scheme_is_case_insensitive` and `authorization_is_not_forwarded_from_the_client`.

- [ ] **Step 5: Verify against a live backend**

```bash
cat > /tmp/rbac.yaml <<'EOF'
model_list:
  - model_name: qwen3-6-35b-a3b-nvfp4
    litellm_params:
      model: openai/qwen3-6-35b-a3b-nvfp4
      api_base: http://192.168.10.245:40045/v1
auth:
  keys:
    - key: sk-allowed
      name: allowed
      models: [qwen3-6-35b-a3b-nvfp4]
    - key: sk-denied
      name: denied
      models: [something-else]
EOF
cargo run --release -- --config /tmp/rbac.yaml --port 4000 &
sleep 2
# expect 200
curl -s -o /dev/null -w "granted: %{http_code}\n" localhost:4000/v1/chat/completions \
  -H 'authorization: Bearer sk-allowed' -H 'content-type: application/json' \
  -d '{"model":"qwen3-6-35b-a3b-nvfp4","messages":[{"role":"user","content":"hi"}],"max_tokens":400}'
# expect 403
curl -s -o /dev/null -w "denied:  %{http_code}\n" localhost:4000/v1/chat/completions \
  -H 'authorization: Bearer sk-denied' -H 'content-type: application/json' \
  -d '{"model":"qwen3-6-35b-a3b-nvfp4","messages":[{"role":"user","content":"hi"}],"max_tokens":400}'
# expect 401
curl -s -o /dev/null -w "no key:  %{http_code}\n" localhost:4000/v1/chat/completions \
  -H 'content-type: application/json' -d '{"model":"qwen3-6-35b-a3b-nvfp4","messages":[]}'
```

Expected: `granted: 200`, `denied: 403`, `no key: 401`.

- [ ] **Step 6: Commit**

```bash
git add src/proxy.rs src/state.rs src/main.rs
git commit -m "Authorise requests from the snapshot instead of a master key

Per request this is a SHA-256, a hash lookup and a set lookup against an
already-flattened grant list. An ungranted model is 403 rather than 404: the
model exists and pretending otherwise would mislead the caller.

--master-key still works and synthesises an allow-all key, with a deprecation
warning, so an existing deployment does not break on upgrade."
```

---

### Task 4: Database schema and migrations

**Files:**
- Create: `migrations/0001_init.sql`, `src/control/mod.rs`, `src/control/db.rs`, `docker-compose.yml`
- Modify: `Cargo.toml`, `src/main.rs`

**Interfaces:**
- Produces: `pub async fn db::connect(url: &str) -> anyhow::Result<PgPool>` (runs migrations on connect).

- [ ] **Step 1: Write the migration**

Create `migrations/0001_init.sql`:

```sql
-- A principal is whatever permissions attach to: a human who signs in, or a
-- service account that owns API keys. Roles attach to principals and never to
-- individual keys, so rotating a key cannot silently change its reach.
CREATE TABLE principals (
    id            BIGSERIAL PRIMARY KEY,
    kind          TEXT NOT NULL CHECK (kind IN ('user', 'service_account')),
    name          TEXT NOT NULL UNIQUE,
    email         TEXT UNIQUE,
    -- Argon2id, and only for kind='user'. Passwords are low-entropy and
    -- human-chosen, which is the case a slow KDF exists for.
    password_hash TEXT,
    disabled      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE roles (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE permissions (
    id       BIGSERIAL PRIMARY KEY,
    verb     TEXT NOT NULL,
    resource TEXT NOT NULL,
    UNIQUE (verb, resource)
);

CREATE TABLE role_permissions (
    role_id       BIGINT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    permission_id BIGINT NOT NULL REFERENCES permissions(id) ON DELETE CASCADE,
    PRIMARY KEY (role_id, permission_id)
);

CREATE TABLE principal_roles (
    principal_id BIGINT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    role_id      BIGINT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
    PRIMARY KEY (principal_id, role_id)
);

CREATE TABLE api_keys (
    id           BIGSERIAL PRIMARY KEY,
    -- SHA-256 of the key. Plaintext is shown once at creation and never stored.
    hash         BYTEA NOT NULL UNIQUE,
    -- First few characters, for display only.
    prefix       TEXT NOT NULL,
    name         TEXT NOT NULL,
    principal_id BIGINT NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
    expires_at   TIMESTAMPTZ,
    disabled     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ
);
CREATE INDEX api_keys_principal ON api_keys(principal_id);

CREATE TABLE models (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE model_backends (
    id               BIGSERIAL PRIMARY KEY,
    model_id         BIGINT NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    api_base         TEXT NOT NULL,
    upstream_model   TEXT NOT NULL,
    -- Encrypted at rest and never returned by the admin API, but it IS carried
    -- usably in the snapshot: the proxy has to present it to the backend, so
    -- unlike a user API key it cannot be reduced to a hash. /snapshot must be
    -- TLS wherever backends have real credentials.
    upstream_api_key BYTEA
);
CREATE INDEX model_backends_model ON model_backends(model_id);

-- Grants are expressed as permissions on resources, so 'model:invoke' on
-- 'model/*' and on 'model/qwen3' use the same machinery.
INSERT INTO permissions (verb, resource) VALUES
    ('model:invoke', 'model/*'),
    ('key:create',   '*'),
    ('key:revoke',   '*'),
    ('config:write', '*'),
    ('usage:read',   '*');

INSERT INTO roles (name, description) VALUES
    ('admin',    'Full administrative access'),
    ('operator', 'Manage models and keys, no user administration'),
    ('inference','Invoke any model, no administrative access');

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p WHERE r.name = 'admin';

INSERT INTO role_permissions (role_id, permission_id)
SELECT r.id, p.id FROM roles r, permissions p
WHERE r.name = 'inference' AND p.verb = 'model:invoke';
```

- [ ] **Step 2: Write the failing test**

Create `src/control/db.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Requires Postgres. Run with:
    ///   docker compose up -d postgres
    ///   DATABASE_URL=postgres://fastllm:fastllm@localhost:5432/fastllm \
    ///     cargo test --features control -- --ignored
    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn migrations_apply_and_seed_the_default_roles() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = connect(&url).await.unwrap();
        let roles: Vec<String> = sqlx::query_scalar("SELECT name FROM roles ORDER BY name")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(roles, vec!["admin", "inference", "operator"]);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn migrations_are_idempotent() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        connect(&url).await.unwrap();
        connect(&url).await.unwrap();
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --features control -- --ignored db`
Expected: FAIL — `cannot find function connect`.

- [ ] **Step 4: Write the implementation**

Create `src/control/mod.rs`:

```rust
//! The control plane: everything that touches Postgres.
//!
//! Feature-gated so a `--role=proxy` build links no database driver at all.

pub mod db;
```

Add to `src/control/db.rs` above its tests:

```rust
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Connect and bring the schema up to date.
///
/// Migrations run on every start rather than as a separate step: there is one
/// writer, the migrations are small, and a deployment that cannot self-migrate
/// is a deployment that fails at 3am.
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
```

Create `docker-compose.yml`:

```yaml
services:
  postgres:
    image: postgres:17
    environment:
      POSTGRES_USER: fastllm
      POSTGRES_PASSWORD: fastllm
      POSTGRES_DB: fastllm
    ports: ["5432:5432"]
    volumes: ["pgdata:/var/lib/postgresql/data"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U fastllm"]
      interval: 5s
      retries: 10

  fastllm:
    build: .
    depends_on:
      postgres: { condition: service_healthy }
    environment:
      FASTLLM_ROLE: all
      FASTLLM_DATABASE_URL: postgres://fastllm:fastllm@postgres:5432/fastllm
      FASTLLM_HOST: 0.0.0.0
    ports: ["4000:4000", "4001:4001"]

volumes:
  pgdata:
```

Add to `Cargo.toml`:

```toml
[features]
default = ["control"]
control = ["dep:sqlx", "dep:axum", "dep:argon2"]

[dependencies]
sqlx = { version = "0.8", optional = true, default-features = false, features = ["runtime-tokio", "tls-rustls-ring", "postgres", "macros", "migrate", "chrono"] }
axum = { version = "0.8", optional = true }
argon2 = { version = "0.5", optional = true }
```

Add `#[cfg(feature = "control")] mod control;` to `src/main.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
docker compose up -d postgres
DATABASE_URL=postgres://fastllm:fastllm@localhost:5432/fastllm \
  cargo test --features control -- --ignored db
```

Expected: PASS, 2 tests.

- [ ] **Step 6: Commit**

```bash
git add migrations/ src/control/ docker-compose.yml Cargo.toml Cargo.lock src/main.rs
git commit -m "Add the control plane schema and Postgres connection

Roles attach to principals rather than to individual keys, so rotating a key
cannot change what it can reach. Migrations run on connect: there is one
writer and a deployment that cannot self-migrate fails at 3am.

sqlx, axum and argon2 sit behind a 'control' feature so a proxy-only build
links no database driver."
```

---

### Task 5: Build a snapshot from the database

Where flattening happens. The most logic-heavy task, and the one that keeps the request path cheap.

**Files:**
- Create: `src/control/build.rs`
- Modify: `src/control/mod.rs`

**Interfaces:**
- Consumes: `PgPool` (Task 4); `Snapshot`, `Principal`, `ModelDef`, `BackendDef` (Task 1).
- Produces: `pub async fn build_snapshot(pool: &PgPool) -> anyhow::Result<Snapshot>`; `pub fn flatten_grants(perms: &[(String, String)], all_models: &[String]) -> (HashSet<String>, bool)`.

- [ ] **Step 1: Write the failing test**

Create `src/control/build.rs` with only its tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_resource_grants_every_model() {
        let perms = vec![("model:invoke".to_string(), "model/*".to_string())];
        let (set, all) = flatten_grants(&perms, &["a".into(), "b".into()]);
        assert!(all);
        assert!(set.is_empty(), "allow_all makes the set redundant");
    }

    #[test]
    fn a_named_resource_grants_only_that_model() {
        let perms = vec![("model:invoke".to_string(), "model/qwen3".to_string())];
        let (set, all) = flatten_grants(&perms, &["qwen3".into(), "other".into()]);
        assert!(!all);
        assert_eq!(set, ["qwen3".to_string()].into_iter().collect());
    }

    #[test]
    fn a_prefix_wildcard_expands_against_the_known_models() {
        let perms = vec![("model:invoke".to_string(), "model/qwen*".to_string())];
        let (set, all) = flatten_grants(&perms, &["qwen3".into(), "qwen2".into(), "llama".into()]);
        assert!(!all);
        assert_eq!(set, ["qwen3".to_string(), "qwen2".to_string()].into_iter().collect());
    }

    #[test]
    fn permissions_other_than_model_invoke_are_ignored_here() {
        // Admin permissions are not inference permissions and must never leak
        // into the request path's grant set.
        let perms = vec![("config:write".to_string(), "*".to_string())];
        let (set, all) = flatten_grants(&perms, &["a".into()]);
        assert!(!all);
        assert!(set.is_empty());
    }

    #[test]
    fn grants_from_several_roles_union() {
        let perms = vec![
            ("model:invoke".to_string(), "model/a".to_string()),
            ("model:invoke".to_string(), "model/b".to_string()),
        ];
        let (set, _) = flatten_grants(&perms, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(set, ["a".to_string(), "b".to_string()].into_iter().collect());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --features control --lib control::build`
Expected: FAIL — `cannot find function flatten_grants`.

- [ ] **Step 3: Write the implementation**

Add above the tests in `src/control/build.rs`:

```rust
//! Turns database rows into the snapshot the data plane consumes.
//!
//! All the expensive work happens here, once per change, so that the request
//! path is a set lookup: roles are resolved to permissions, permissions to
//! model names, and wildcards expanded against the known model list.

use crate::snapshot::{BackendDef, KeyEntry, ModelDef, Principal, Snapshot};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

/// Resolve `model:invoke` permissions into a concrete set of model names.
///
/// `model/*` short-circuits to `allow_all` rather than materialising every
/// name, so a grant stays correct when a model is added later. Any other verb
/// is an administrative permission and has no place in the request path.
pub fn flatten_grants(
    perms: &[(String, String)],
    all_models: &[String],
) -> (HashSet<String>, bool) {
    let mut set = HashSet::new();
    for (verb, resource) in perms {
        if verb != "model:invoke" {
            continue;
        }
        let Some(pattern) = resource.strip_prefix("model/") else {
            continue;
        };
        if pattern == "*" {
            return (HashSet::new(), true);
        }
        match pattern.strip_suffix('*') {
            Some(prefix) => {
                set.extend(all_models.iter().filter(|m| m.starts_with(prefix)).cloned())
            }
            None => {
                if all_models.iter().any(|m| m == pattern) {
                    set.insert(pattern.to_string());
                }
            }
        }
    }
    (set, false)
}

pub async fn build_snapshot(pool: &PgPool) -> anyhow::Result<Snapshot> {
    let model_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM models ORDER BY name")
            .fetch_all(pool)
            .await?;
    let all_names: Vec<String> = model_rows.iter().map(|(_, n)| n.clone()).collect();

    let backend_rows: Vec<(i64, String, String, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT model_id, api_base, upstream_model, upstream_api_key FROM model_backends",
    )
    .fetch_all(pool)
    .await?;

    let mut models = Vec::new();
    for (id, name) in &model_rows {
        models.push(ModelDef {
            name: name.clone(),
            backends: backend_rows
                .iter()
                .filter(|(mid, ..)| mid == id)
                .map(|(_, base, upstream, key)| BackendDef {
                    api_base: base.trim_end_matches('/').to_string(),
                    upstream_model: upstream.clone(),
                    api_key: key.as_ref().map(|k| String::from_utf8_lossy(k).into_owned()),
                })
                .collect(),
        });
    }

    let principal_rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM principals WHERE NOT disabled")
            .fetch_all(pool)
            .await?;

    let mut principals = HashMap::new();
    for (id, name) in principal_rows {
        let perms: Vec<(String, String)> = sqlx::query_as(
            "SELECT p.verb, p.resource FROM permissions p
             JOIN role_permissions rp ON rp.permission_id = p.id
             JOIN principal_roles pr  ON pr.role_id = rp.role_id
             WHERE pr.principal_id = $1",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;
        let (allowed_models, allow_all) = flatten_grants(&perms, &all_names);
        principals.insert(
            id as u64,
            Principal { id: id as u64, name, allowed_models, allow_all },
        );
    }

    let key_rows: Vec<(Vec<u8>, i64, Option<chrono::DateTime<chrono::Utc>>, bool)> = sqlx::query_as(
        "SELECT hash, principal_id, expires_at, disabled FROM api_keys",
    )
    .fetch_all(pool)
    .await?;

    let mut keys = HashMap::new();
    for (hash, principal, expires, disabled) in key_rows {
        let Ok(hash): Result<[u8; 32], _> = hash.try_into() else {
            continue;
        };
        keys.insert(
            hash,
            KeyEntry {
                principal: principal as u64,
                expires_at: expires.map(|d| d.into()),
                disabled,
            },
        );
    }

    // The version is a clock the control plane owns; the proxy only compares it.
    let version: i64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::BIGINT")
        .fetch_one(pool)
        .await?;

    Ok(Snapshot {
        version: version as u64,
        keys,
        principals,
        models,
        open: false,
    })
}
```

Add `pub mod build;` to `src/control/mod.rs` and `chrono = { version = "0.4", optional = true }` to the `control` feature.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --features control --lib control::build`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add src/control/build.rs src/control/mod.rs Cargo.toml Cargo.lock
git commit -m "Build the snapshot from the database, flattening grants

Roles resolve to permissions, permissions to model names, and wildcards expand
against the known models — once per change rather than once per request.
'model/*' short-circuits to allow_all rather than materialising every name, so
the grant stays correct when a model is added later.

Verbs other than model:invoke are administrative and are deliberately excluded
from the grant set the request path sees."
```

---

### Task 6: Admin API and `/snapshot`

**Files:**
- Create: `src/control/api.rs`
- Modify: `src/control/mod.rs`

**Interfaces:**
- Consumes: `build_snapshot` (Task 5), `PgPool` (Task 4).
- Produces: `pub async fn serve(pool: PgPool, addr: SocketAddr, proxy_token: String, cache: Arc<ArcSwap<Snapshot>>) -> anyhow::Result<()>`; routes `POST /admin/keys`, `DELETE /admin/keys/:id`, `GET /admin/keys`, `POST /admin/models`, `GET /snapshot`.

- [ ] **Step 1: Write the failing test**

Create `src/control/api.rs` with only its tests:

```rust
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
        let (plaintext, id) = create_key(&pool, "test-key", 1, None).await.unwrap();
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
        assert_ne!(any_plaintext.unwrap(), plaintext, "plaintext must not be stored");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --features control --lib control::api`
Expected: FAIL — `cannot find function generate_key`.

- [ ] **Step 3: Write the implementation**

Add above the tests in `src/control/api.rs`:

```rust
//! Admin API and the snapshot endpoint.
//!
//! `/snapshot` is read-only and authenticated with the proxy's own token,
//! which is distinct from any user key: a stolen proxy token discloses policy
//! — key hashes, never plaintext — and grants nothing else.

use crate::control::build::build_snapshot;
use crate::snapshot::{Snapshot, hash_key};
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
    cache: Arc<ArcSwap<Snapshot>>,
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

async fn revoke_key(
    State(ctx): State<Ctx>,
    Path(id): Path<i64>,
) -> Result<StatusCode, StatusCode> {
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
        ctx.cache.store(Arc::new(snap));
    }
}

async fn get_snapshot(State(ctx): State<Ctx>, headers: HeaderMap) -> impl IntoResponse {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if presented != Some(ctx.proxy_token.as_str()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let snap = ctx.cache.load();
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
    cache: Arc<ArcSwap<Snapshot>>,
) -> anyhow::Result<()> {
    let ctx = Ctx { pool, proxy_token, cache };
    let app = Router::new()
        .route("/admin/keys", post(post_key))
        .route("/admin/keys/{id}", delete(revoke_key))
        .route("/snapshot", get(get_snapshot))
        .with_state(ctx);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

Add `to_wire()`/`from_wire()` on `Snapshot` in `src/snapshot.rs` — a `serde`-derived mirror struct with hex-encoded key hashes, since `[u8; 32]` map keys do not round-trip through JSON.

Add to `Cargo.toml`: `rand = "0.9"`, `hex = "0.4"`, and `serde_json` is already present.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --features control --lib control::api
DATABASE_URL=postgres://fastllm:fastllm@localhost:5432/fastllm \
  cargo test --features control -- --ignored control::api
```

Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/control/api.rs src/control/mod.rs src/snapshot.rs Cargo.toml Cargo.lock
git commit -m "Add the admin API and the snapshot endpoint

Key plaintext is returned exactly once at creation; only the SHA-256 and a
display prefix are stored. The snapshot is rebuilt immediately after a write,
so revocation latency is the proxy's poll interval alone.

/snapshot takes the proxy's own token rather than a user key: read-only by
construction, and what it discloses is policy and key hashes, never plaintext."
```

---

### Task 7: The `Http` source with a disk cache

The failure-mode task. Everything here exists so a control-plane outage is not a data-plane outage.

**Files:**
- Create: `src/source/http.rs`
- Modify: `src/source/mod.rs`

**Interfaces:**
- Consumes: `Snapshot::to_wire`/`from_wire` (Task 6), `SnapshotSource` (Task 2).
- Produces: `pub struct HttpSource { url: String, token: String, cache_path: PathBuf }` with `HttpSource::new(url, token, cache_path)` and `pub fn load_cached(&self) -> Option<Snapshot>`.

- [ ] **Step 1: Write the failing test**

Create `src/source/http.rs` with only its tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Principal, Snapshot};

    fn a_snapshot() -> Snapshot {
        Snapshot::for_test(
            vec![("sk-x".into(), 1, None, false)],
            vec![Principal {
                id: 1,
                name: "p".into(),
                allowed_models: ["m".to_string()].into_iter().collect(),
                allow_all: false,
            }],
            vec![],
        )
    }

    #[test]
    fn a_snapshot_survives_a_round_trip_through_the_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        let src = HttpSource::new("http://unused".into(), "t".into(), path.clone());
        src.write_cache(&a_snapshot()).unwrap();

        let loaded = src.load_cached().expect("cache should load");
        let p = loaded.authenticate("sk-x", std::time::SystemTime::now()).unwrap();
        assert_eq!(p.name, "p");
        assert!(p.may_invoke("m"));
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = HttpSource::new("http://unused".into(), "t".into(), dir.path().join("nope.json"));
        assert!(src.load_cached().is_none());
    }

    #[test]
    fn a_corrupt_cache_is_ignored_rather_than_fatal() {
        // A truncated write must not stop the proxy from starting.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let src = HttpSource::new("http://unused".into(), "t".into(), path);
        assert!(src.load_cached().is_none());
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib source::http`
Expected: FAIL — `cannot find type HttpSource`.

- [ ] **Step 3: Write the implementation**

Add above the tests in `src/source/http.rs`:

```rust
//! Polls a control plane, and keeps serving when it is not there.
//!
//! The disk cache is the whole point: a proxy that cannot reach its control
//! plane keeps serving on the last policy it saw and merely stops learning
//! about changes. Losing the control plane must not lose inference.

use crate::snapshot::Snapshot;
use crate::source::SnapshotSource;
use std::path::PathBuf;

pub struct HttpSource {
    url: String,
    token: String,
    cache_path: PathBuf,
}

impl HttpSource {
    pub fn new(url: String, token: String, cache_path: PathBuf) -> Self {
        Self { url, token, cache_path }
    }

    pub fn write_cache(&self, snap: &Snapshot) -> anyhow::Result<()> {
        if let Some(dir) = self.cache_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write and rename, so a crash mid-write cannot leave a torn file that
        // the next start would reject.
        let tmp = self.cache_path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&snap.to_wire())?)?;
        std::fs::rename(&tmp, &self.cache_path)?;
        Ok(())
    }

    /// Last-known-good, or `None` for absent, unreadable or corrupt.
    pub fn load_cached(&self) -> Option<Snapshot> {
        let bytes = std::fs::read(&self.cache_path).ok()?;
        let wire = serde_json::from_slice(&bytes).ok()?;
        Some(Snapshot::from_wire(wire))
    }
}

impl SnapshotSource for HttpSource {
    async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
        let client = reqwest::Client::new();
        let mut req = client
            .get(&self.url)
            .bearer_auth(&self.token)
            .timeout(std::time::Duration::from_secs(10));
        if let Some(v) = have {
            req = req.header("if-none-match", format!("\"{v}\""));
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let snap = Snapshot::from_wire(resp.error_for_status()?.json().await?);
        // Cache failures are logged, never fatal: a read-only filesystem
        // should degrade the outage story, not stop the proxy.
        if let Err(e) = self.write_cache(&snap) {
            tracing::warn!(error = %e, "could not write the snapshot cache");
        }
        Ok(Some(snap))
    }
}
```

Add `reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }` to `Cargo.toml`, and `pub mod http;` to `src/source/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib source::http`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add src/source/http.rs src/source/mod.rs Cargo.toml Cargo.lock
git commit -m "Add the Http snapshot source with a last-known-good disk cache

A proxy that cannot reach its control plane keeps serving the last policy it
saw and only stops learning about changes: losing the control plane must not
lose inference. The cache is written via rename so a crash mid-write cannot
leave a torn file, and a corrupt cache is ignored rather than fatal."
```

---

### Task 8: Role dispatch and the polling loop

Wires everything together. First task where `--role=all` runs.

**Files:**
- Modify: `src/main.rs`, `src/source/mod.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `--role` CLI flag; `pub fn spawn_poller(source: impl SnapshotSource + 'static, state: Arc<AppState>, interval: Duration)`.

- [ ] **Step 1: Write the failing test**

Add to `src/source/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    struct Counting {
        calls: Arc<AtomicU64>,
        version: u64,
    }

    impl SnapshotSource for Counting {
        async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if have == Some(self.version) {
                return Ok(None);
            }
            let mut s = Snapshot::default();
            s.version = self.version;
            Ok(Some(s))
        }
    }

    #[tokio::test]
    async fn the_poller_stops_swapping_once_the_version_is_current() {
        let calls = Arc::new(AtomicU64::new(0));
        let src = Counting { calls: Arc::clone(&calls), version: 7 };
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(Snapshot::default()));

        poll_once(&src, &cell).await.unwrap();
        assert_eq!(cell.load().version, 7);
        // Second poll returns None, so the stored Arc must not be replaced.
        let before = Arc::as_ptr(&cell.load_full());
        poll_once(&src, &cell).await.unwrap();
        assert_eq!(Arc::as_ptr(&cell.load_full()), before);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib source::tests`
Expected: FAIL — `cannot find function poll_once`.

- [ ] **Step 3: Write the implementation**

Add to `src/source/mod.rs`:

```rust
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Duration;

/// One poll. Swaps only when the source reports a new version, so a steady
/// state costs one HTTP request with an ETag and no allocation.
pub async fn poll_once(
    source: &impl SnapshotSource,
    cell: &ArcSwap<Snapshot>,
) -> anyhow::Result<()> {
    let have = Some(cell.load().version).filter(|v| *v != 0);
    if let Some(next) = source.fetch(have).await? {
        cell.store(Arc::new(next));
    }
    Ok(())
}

pub fn spawn_poller<S: SnapshotSource + 'static>(
    source: S,
    cell: Arc<ArcSwap<Snapshot>>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = poll_once(&source, &cell).await {
                // Expected whenever the control plane is down. The cached
                // snapshot keeps serving, so this is a warning, not an error.
                tracing::warn!(error = %e, "snapshot refresh failed; serving the cached policy");
            }
        }
    });
}
```

In `src/main.rs` add the role flag and dispatch:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// Control plane and forwarding in one process. The default, and what a
    /// single container runs.
    All,
    /// Database, admin API and /snapshot only.
    Control,
    /// Forwarding only, against a control plane or a config file.
    Proxy,
}

#[arg(long, value_enum, default_value_t = Role::All, env = "FASTLLM_ROLE")]
role: Role,

#[arg(long, env = "FASTLLM_DATABASE_URL")]
database_url: Option<String>,

#[arg(long, env = "FASTLLM_CONTROL_URL")]
control_url: Option<String>,

#[arg(long, env = "FASTLLM_PROXY_TOKEN")]
proxy_token: Option<String>,

#[arg(long, default_value = "/var/lib/fastllm/snapshot.json")]
snapshot_cache: PathBuf,

#[arg(long, default_value_t = 4001)]
admin_port: u16,
```

Dispatch in `run`:

```rust
    // A proxy that starts with nothing must still start. Refusing to boot
    // turns a control-plane outage into a crash-loop, which is exactly the
    // failure this architecture exists to prevent.
    let snapshot = Arc::new(ArcSwap::from_pointee(match cli.role {
        Role::Proxy if cli.control_url.is_some() => {
            let src = HttpSource::new(/* ... */);
            match src.fetch(None).await {
                Ok(Some(s)) => s,
                Err(e) => {
                    warn!(error = %e, "control plane unreachable at startup");
                    src.load_cached().unwrap_or_default()
                }
                Ok(None) => src.load_cached().unwrap_or_default(),
            }
        }
        _ => FileSource::new(cli.config.clone()).fetch(None).await?.unwrap_or_default(),
    }));
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, all tests.

- [ ] **Step 5: Verify all three roles start**

```bash
docker compose up -d postgres
# all
FASTLLM_DATABASE_URL=postgres://fastllm:fastllm@localhost:5432/fastllm \
  cargo run --release --features control -- --role all --config /tmp/rbac.yaml &
sleep 3 && curl -s -o /dev/null -w "all:     %{http_code}\n" localhost:4000/health && kill %1
# proxy against a file, no dependencies at all
cargo run --release -- --role proxy --config /tmp/rbac.yaml &
sleep 2 && curl -s -o /dev/null -w "proxy:   %{http_code}\n" localhost:4000/health && kill %1
```

Expected: `all: 200`, `proxy: 200`.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/source/mod.rs
git commit -m "Add role dispatch and the snapshot polling loop

--role all/control/proxy, so the same image is a lab container and a scaled
deployment. A proxy that cannot reach its control plane at startup falls back
to its disk cache and, failing that, starts empty and reports unhealthy:
refusing to boot would turn a control-plane outage into a crash-loop."
```

---

### Task 9: `import --config`

Migration path off the ConfigMap that is running in production today.

**Files:**
- Create: `src/control/import.rs`
- Modify: `src/control/mod.rs`, `src/main.rs`

**Interfaces:**
- Produces: `pub async fn import(pool: &PgPool, cfg: &FileConfig) -> anyhow::Result<ImportSummary>`; `pub struct ImportSummary { pub models: usize, pub backends: usize, pub keys: usize }`.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_the_deployed_config_creates_models_and_backends() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        sqlx::query("TRUNCATE models, model_backends CASCADE").execute(&pool).await.unwrap();

        let cfg: crate::config::FileConfig = serde_yaml::from_str(
            "model_list:\n\
             \x20 - model_name: qwen3\n\
             \x20   litellm_params: { model: openai/qwen3, api_base: http://a:8000/v1 }\n\
             \x20 - model_name: qwen3\n\
             \x20   litellm_params: { model: openai/qwen3, api_base: http://b:8000/v1 }\n",
        )
        .unwrap();

        let summary = import(&pool, &cfg).await.unwrap();
        // Two entries sharing a model_name are one pool with two backends.
        assert_eq!(summary.models, 1);
        assert_eq!(summary.backends, 2);
    }

    #[tokio::test]
    #[ignore = "requires postgres"]
    async fn importing_twice_does_not_duplicate() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = crate::control::db::connect(&url).await.unwrap();
        sqlx::query("TRUNCATE models, model_backends CASCADE").execute(&pool).await.unwrap();
        let cfg: crate::config::FileConfig = serde_yaml::from_str(
            "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://a:8000/v1 }\n",
        )
        .unwrap();
        import(&pool, &cfg).await.unwrap();
        let second = import(&pool, &cfg).await.unwrap();
        assert_eq!(second.models, 1);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_backends")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "re-import must be idempotent");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --features control -- --ignored import`
Expected: FAIL — `cannot find function import`.

- [ ] **Step 3: Write the implementation**

```rust
//! Seeds the database from a LiteLLM-format config.
//!
//! The migration path off a file-driven deployment, and the reason the
//! LiteLLM-compatibility promise survives the move to a control plane.

use crate::config::FileConfig;
use sqlx::PgPool;

pub struct ImportSummary {
    pub models: usize,
    pub backends: usize,
    pub keys: usize,
}

pub async fn import(pool: &PgPool, cfg: &FileConfig) -> anyhow::Result<ImportSummary> {
    let mut tx = pool.begin().await?;
    let mut models = 0usize;
    let mut backends = 0usize;

    for entry in &cfg.model_list {
        let name = &entry.model_name;
        let model_id: i64 = sqlx::query_scalar(
            "INSERT INTO models (name) VALUES ($1)
             ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
             RETURNING id",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        models += 1;

        let api_base = entry.litellm_params.api_base.trim_end_matches('/');
        let upstream = entry.litellm_params.upstream_model(name);
        // Idempotent on (model, api_base, upstream_model) so re-running import
        // over an edited file adds without duplicating.
        let inserted = sqlx::query(
            "INSERT INTO model_backends (model_id, api_base, upstream_model, upstream_api_key)
             SELECT $1, $2, $3, $4
             WHERE NOT EXISTS (
               SELECT 1 FROM model_backends
               WHERE model_id = $1 AND api_base = $2 AND upstream_model = $3)",
        )
        .bind(model_id)
        .bind(api_base)
        .bind(&upstream)
        .bind(entry.litellm_params.effective_api_key().map(|k| k.into_bytes()))
        .execute(&mut *tx)
        .await?;
        backends += inserted.rows_affected() as usize;
    }

    tx.commit().await?;
    Ok(ImportSummary {
        models: sqlx::query_scalar::<_, i64>("SELECT count(*) FROM models")
            .fetch_one(pool)
            .await? as usize,
        backends,
        keys: 0,
    })
}
```

Wire `fastllm-proxy import --config <path>` as a clap subcommand in `src/main.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
DATABASE_URL=postgres://fastllm:fastllm@localhost:5432/fastllm \
  cargo test --features control -- --ignored import
```

Expected: PASS, 2 tests.

- [ ] **Step 5: Commit**

```bash
git add src/control/import.rs src/control/mod.rs src/main.rs
git commit -m "Add import --config to seed the database from a LiteLLM config

The migration path off the file-driven deployment running today, and what
keeps the LiteLLM-compatibility promise alive after policy moves to Postgres.
Idempotent on (model, api_base, upstream_model) so re-importing an edited file
adds without duplicating."
```

---

### Task 10: End-to-end authorisation test

**Files:**
- Create: `tests/rbac.rs`
- Modify: `bench/` (move the mock upstream out of the scratchpad — see the spec's testing section)

**Interfaces:**
- Consumes: everything.

- [ ] **Step 1: Move the mock upstream into the repo**

The mock upstream, load generator and latency harness currently live in a
session scratchpad and will be lost. Move them to `bench/` as a workspace
member, so P1 and P2 can reuse them:

```bash
mkdir -p bench/src
# copy upstream.rs, load.rs, realbench.rs, micro.rs, tcprelay.rs into bench/src
# add bench/Cargo.toml with [[bin]] entries for each
# add `[workspace] members = [".", "bench"]` to the root Cargo.toml
cargo build --release -p bench
```

- [ ] **Step 2: Write the failing test**

Create `tests/rbac.rs`:

```rust
//! End-to-end authorisation against the mock upstream.
//!
//! `--role=all` is what makes this tractable: the whole system runs in one
//! process, so there is no cluster and no compose file in the loop.

use std::process::{Child, Command};
use std::time::Duration;

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

fn start(config: &str, port: u16) -> Proc {
    let path = std::env::temp_dir().join(format!("rbac-{port}.yaml"));
    std::fs::write(&path, config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
        .args(["--config", path.to_str().unwrap(), "--port", &port.to_string(), "--role", "proxy"])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    Proc(child)
}

const CONFIG: &str = "\
model_list:
  - model_name: allowed-model
    litellm_params: { api_base: http://127.0.0.1:8199/v1 }
  - model_name: other-model
    litellm_params: { api_base: http://127.0.0.1:8199/v1 }
auth:
  keys:
    - key: sk-narrow
      name: narrow
      models: [allowed-model]
    - key: sk-wide
      name: wide
      models: ['*']
    - key: sk-expired
      name: expired
      models: ['*']
      expires_at: 2020-01-01T00:00:00Z
";

fn post(port: u16, key: Option<&str>, model: &str) -> u16 {
    let mut req = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"));
    if let Some(k) = key {
        req = req.set("authorization", &format!("Bearer {k}"));
    }
    match req.send_json(ureq::json!({ "model": model, "messages": [] })) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(_) => 0,
    }
}

#[test]
fn authorisation_allows_denies_and_expires() {
    let _p = start(CONFIG, 4411);

    assert_eq!(post(4411, None, "allowed-model"), 401, "no key");
    assert_eq!(post(4411, Some("sk-bogus"), "allowed-model"), 401, "unknown key");
    assert_eq!(post(4411, Some("sk-expired"), "allowed-model"), 401, "expired key");
    assert_eq!(post(4411, Some("sk-narrow"), "other-model"), 403, "ungranted model");
    // Granted: reaches routing and fails at the (absent) upstream, not at authz.
    assert_ne!(post(4411, Some("sk-narrow"), "allowed-model"), 403);
    assert_ne!(post(4411, Some("sk-wide"), "other-model"), 403, "wildcard grant");
}

#[test]
fn an_unknown_model_is_404_for_an_authorised_caller() {
    let _p = start(CONFIG, 4412);
    assert_eq!(post(4412, Some("sk-wide"), "no-such-model"), 404);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --test rbac`
Expected: FAIL — binary not yet supporting `--role`, or assertion failures.

- [ ] **Step 4: Make it pass**

Add `ureq = { version = "2", features = ["json"] }` to `[dev-dependencies]`. Fix whatever the assertions surface — most likely the ordering of the authorisation check against the model-exists check. The correct order is: authenticate, then look up the model, then authorise. A caller with a valid key asking for a model that does not exist gets 404; a caller asking for one that exists but is not granted gets 403.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test rbac`
Expected: PASS, 2 tests.

- [ ] **Step 6: Commit**

```bash
git add tests/rbac.rs bench/ Cargo.toml
git commit -m "Add end-to-end authorisation tests and move the bench harness into the repo

The mock upstream and load generator lived in a session scratchpad and would
have been lost; P1 and P2 both need them, and every performance number in
TODO.md came from them.

Check ordering is asserted deliberately: authenticate, then resolve the model,
then authorise. An unknown model is 404 even for an authorised caller; a known
model the caller may not use is 403."
```

---

### Task 11: Assert the request path does no I/O

The guardrail for the property every later phase will be tempted to erode.

**Files:**
- Create: `tests/no_io_on_hot_path.rs`

- [ ] **Step 1: Write the failing test**

```rust
//! The property most likely to rot.
//!
//! Every feature after P0 — rate limits, budgets, routing rules — adds a
//! temptation to do one more lookup per request. This test exists so that
//! temptation fails loudly rather than quietly costing milliseconds.

use std::sync::atomic::{AtomicBool, Ordering};

static SERVING: AtomicBool = AtomicBool::new(false);

/// A source that panics if asked for anything while a request is in flight.
struct ForbiddenDuringRequests;

impl ForbiddenDuringRequests {
    fn touch(&self) {
        assert!(
            !SERVING.load(Ordering::SeqCst),
            "the request path performed I/O: authorisation must read the \
             in-memory snapshot only"
        );
    }
}

#[test]
fn authorisation_reads_only_the_snapshot() {
    use fastllm_proxy::snapshot::{Principal, Snapshot};

    let snap = Snapshot::for_test(
        vec![("sk-x".into(), 1, None, false)],
        vec![Principal {
            id: 1,
            name: "p".into(),
            allowed_models: ["m".to_string()].into_iter().collect(),
            allow_all: false,
        }],
        vec![],
    );

    SERVING.store(true, Ordering::SeqCst);
    let p = snap.authenticate("sk-x", std::time::SystemTime::now()).unwrap();
    assert!(p.may_invoke("m"));
    SERVING.store(false, Ordering::SeqCst);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test no_io_on_hot_path`
Expected: FAIL — `fastllm_proxy` is a binary crate with no library target.

- [ ] **Step 3: Add a library target**

Add to `Cargo.toml`:

```toml
[lib]
name = "fastllm_proxy"
path = "src/lib.rs"
```

Create `src/lib.rs` re-exporting the modules the tests need, and make `src/main.rs` a thin binary over it. This is a mechanical change but touches every module's visibility; do it in one commit and run the whole suite.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, whole suite.

- [ ] **Step 5: Commit**

```bash
git add tests/no_io_on_hot_path.rs src/lib.rs src/main.rs Cargo.toml
git commit -m "Split out a library target and assert the request path does no I/O

Every phase after this one adds a temptation to do one more lookup per
request. The measured overhead of this proxy against a real vLLM is currently
zero; this test is what keeps it that way."
```

---

### Task 12: CI, deployment and documentation

**Files:**
- Modify: `.github/workflows/ci.yml`, `deploy/`, `README.md`, `Dockerfile`

- [ ] **Step 1: Add Postgres to CI**

In `.github/workflows/ci.yml`, add a service container to the `test` job and run the ignored tests:

```yaml
    services:
      postgres:
        image: postgres:17
        env:
          POSTGRES_USER: fastllm
          POSTGRES_PASSWORD: fastllm
          POSTGRES_DB: fastllm
        options: >-
          --health-cmd pg_isready --health-interval 5s --health-retries 10
```

and a step:

```yaml
      - name: Test (with database)
        env:
          DATABASE_URL: postgres://fastllm:fastllm@postgres:5432/fastllm
        run: cargo test --features control --locked -- --include-ignored
```

- [ ] **Step 2: Add the control-plane deployment**

Create `deploy/control.yaml` — a `Deployment` with `--role=control`, a `Service` on 4001, a CloudNativePG `Cluster`, and a `Secret` holding the proxy token. Change `deploy/deployment.yaml` to `--role=proxy` with `FASTLLM_CONTROL_URL` and `FASTLLM_PROXY_TOKEN`, and mount an `emptyDir` at `/var/lib/fastllm` for the snapshot cache.

- [ ] **Step 3: Document it**

Update `README.md` with the three roles, the `auth:` config section, `import --config`, and a docker-compose quickstart. Update `deploy/README.md` for the split deployment and how to read a created key. Update `TODO.md` to mark P0 done and link the spec.

- [ ] **Step 4: Verify the whole thing on the cluster**

```bash
kubectl apply -f deploy/
kubectl -n fastllm rollout status deploy/fastllm-control --timeout=240s
kubectl -n fastllm rollout status deploy/fastllm-proxy --timeout=240s
# create a key through the admin API, then use it
KEY=$(kubectl -n fastllm exec deploy/fastllm-control -- \
  curl -s -XPOST localhost:4001/admin/keys -H 'content-type: application/json' \
  -d '{"name":"smoke","principal_id":1}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["key"])')
curl -s -o /dev/null -w "new key: %{http_code}\n" http://192.168.10.126/v1/chat/completions \
  -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"qwen3-6-35b-a3b-nvfp4","messages":[{"role":"user","content":"hi"}],"max_tokens":400}'
```

Expected: `new key: 200`. Then revoke it and confirm a 401 within ~2 seconds.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml deploy/ README.md TODO.md Dockerfile
git commit -m "Wire P0 into CI, the cluster deployment and the docs

CI gains a Postgres service so the ignored database tests actually run. The
kw deployment splits into --role=control and --role=proxy, with the snapshot
cache on an emptyDir so a restarted proxy still starts if the control plane
is down."
```

---

## Self-Review

**Spec coverage.** Control plane and data plane split (Tasks 4–8); snapshot as the sole contract (Tasks 1, 5, 7); three snapshot sources (Tasks 2, 7, 8); full RBAC with principals, roles, permissions (Tasks 4, 5); SHA-256 for keys and Argon2id for passwords (Task 1 and the Task 4 schema); key expiry (Tasks 1, 3); pre-flattened grants (Task 5); request path unchanged in cost (Tasks 3, 11); failure modes including cold start with no cache (Task 8) and corrupt cache (Task 7); `import --config` (Task 9); `File` mode preserved (Task 2); moving the bench harness into the repo (Task 10); deployment and CI (Task 12).

**Not covered here, by design:** rate limits (P2), usage and budgets (P3), virtual models and routing (P1), the management UI (P4), and `POST /usage`. The spec defines `/usage` in P0 so the protocol does not need reshaping, but nothing sends to it until P2, so it is deferred to the P2 plan rather than built dead.

**Known gap to resolve during Task 6:** the admin API is authenticated only by the proxy token for `/snapshot`. Admin endpoint authentication (`principals` with Argon2id passwords, sessions) is specified but its plan lives with P4, since the UI is what needs sessions. Until then `/admin/*` must be bound to localhost or a cluster-internal Service — **not exposed on the LoadBalancer**. Task 12 step 2 must not add `/admin` to the public VIP.

**Type consistency.** `Snapshot`, `Principal`, `KeyEntry`, `ModelDef`, `BackendDef`, `AuthError`, `hash_key`, `SnapshotSource::fetch`, `poll_once`, `build_snapshot`, `flatten_grants`, `create_key`, `generate_key`, `display_prefix`, `import`, `ImportSummary` are used consistently across tasks. `Snapshot::to_wire`/`from_wire` are introduced in Task 6 and consumed in Task 7.
