//! Cleanup helper for the Postgres-backed integration tests in `tests/`
//! (`budgets.rs`, `frontend_models.rs`, and any other that touches the
//! database). Identical in spirit to `src/control/test_support.rs`, which
//! the unit tests in `src/control/*.rs` use instead — that module is
//! `#[cfg(test)]`-only and not visible to these integration test binaries
//! (each of which links `fastllm_proxy` as an external crate), so it is
//! duplicated here rather than shared.
//!
//! These tests run against the shared kw dev-cluster Postgres, not a
//! throwaway container — there is no `docker compose down -v` to reset it
//! between runs. Left unchecked, every unique-timestamped row a test creates
//! (`unique_name("vm-primary")` and friends) accumulates forever: one run
//! that did this left 311 junk models, 360 junk principals and 223 junk
//! keys behind, and eventually every snapshot rebuild logged an error per
//! undecryptable leftover row, enough log volume to blow a 10s startup
//! timeout in an unrelated test. That is what this guard exists to prevent,
//! not general tidiness.
//!
//! [`TestCleanup`] deletes rows by table + column + exact value/prefix on
//! `Drop`, so cleanup runs even when the test body panics (an `assert!`
//! failure is the *common* case this needs to survive, not the exception) —
//! a cleanup statement at the end of the test function would simply never
//! run in that case. Track only the unique name/prefix a test itself
//! generated; never issue an unqualified `DELETE FROM`, because the
//! database also holds real rows this suite must not touch: the registered
//! model `qwen3-6-35b-a3b-nvfp4` and the bootstrap admin principal.
// Not every test in this module needs every tracking method.
#![allow(dead_code)]

/// A fixed password and its precomputed Argon2id hash (P4), shared by every
/// integration test that needs a real admin session to call `/admin/*` —
/// which, since `require_session` (`src/control/api.rs`) started gating
/// every one of those routes, is now all of them. Precomputed rather than
/// calling `control::auth::hash_password` at test time: this whole file
/// works through plain `sqlx`, never `fastllm_proxy::control::*` directly
/// (see the module doc comment above), which is what keeps it compiling
/// under `cargo build --no-default-features`. Regenerate with that function
/// if this password ever changes.
pub const TEST_PASSWORD: &str = "correct horse battery staple";
pub const TEST_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ADQjtj0AshO4tt+y8yYwgA$b9DCigRtGIAy9y9xRa9Lip7A9pBHRU+4AMK9lJkXDOA";

/// Create a `kind = 'user'` principal named `name` with [`TEST_PASSWORD`]
/// already set and the `admin` role granted, so a caller can immediately
/// `login_cookie` as it and use every `/admin/*` route. `/admin/*` is now
/// gated on both a valid session (`require_session`) *and* a permission the
/// matched route requires (`RequirePermission` — see `src/control/api.rs`),
/// so a caller of this helper that meant "give me a working admin session
/// for some other test" (`budgets.rs`, `frontend_models.rs`) would otherwise
/// start getting 403s that test was never meant to exercise. A test that
/// specifically wants a *narrower* principal — the RBAC finding this helper
/// predates — grants its own role directly instead of using this one; see
/// `admin_auth.rs`'s permission-mapping tests.
pub async fn bootstrap_login_user(pool: &sqlx::PgPool, name: &str) {
    let principal_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO principals (kind, name, password_hash) VALUES ('user', $1, $2) RETURNING id",
    )
    .bind(name)
    .bind(TEST_PASSWORD_HASH)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO principal_roles (principal_id, role_id)
         SELECT $1, id FROM roles WHERE name = 'admin'",
    )
    .bind(principal_id)
    .execute(pool)
    .await
    .unwrap();
}

/// `POST /login` as `name`/[`TEST_PASSWORD`] against a running admin API and
/// return the `Cookie`-ready `fastllm_session=...` value — call
/// `bootstrap_login_user` first so the principal (and its password) exist.
pub fn login_cookie(admin_port: u16, name: &str) -> String {
    // Retried, because the admin API listens on a *different* port from the
    // one the callers' `wait_healthy` polls: a proxy answering `/health` has
    // not necessarily bound its admin listener yet. That race was always here
    // and only started failing once enough test binaries ran in parallel to
    // widen the window — which is the worst way for a harness bug to announce
    // itself, since it looks like a product failure in whichever test lost.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last;
    loop {
        match ureq::post(&format!("http://127.0.0.1:{admin_port}/login"))
            .send_json(ureq::json!({ "name": name, "password": TEST_PASSWORD }))
        {
            Ok(resp) => {
                let raw = resp
                    .header("set-cookie")
                    .expect("login must set a session cookie")
                    .to_string();
                return raw.split(';').next().unwrap().trim().to_string();
            }
            Err(e) => last = e.to_string(),
        }
        if std::time::Instant::now() >= deadline {
            panic!("admin login on port {admin_port} never succeeded within 30s: {last}");
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Deletes tracked rows on drop. Foreign keys from `models`,
/// `frontend_model`-family tables, `api_keys`, `principal_roles`, `limits`,
/// `budgets` and `usage_events` are all `ON DELETE CASCADE` back to
/// `models`/`frontend_models`/`principals` (see the migrations), so tracking
/// just the parent row is enough — the join/child rows go with it.
pub struct TestCleanup {
    // (table, column, pattern) in the order they should be deleted.
    deletions: Vec<(&'static str, &'static str, String)>,
}

impl TestCleanup {
    /// Reads `DATABASE_URL` lazily, only if `drop` ends up with something to
    /// clean — not here, so building one of these before a test has even
    /// confirmed Postgres is reachable stays cheap and infallible.
    pub fn new() -> Self {
        Self {
            deletions: Vec::new(),
        }
    }

    /// Track a `DELETE FROM <table> WHERE <column> LIKE '<prefix>%'`. Use
    /// the exact tag passed to `unique_name`/`unique_name`-alike helpers so
    /// this can never match a real row: real rows don't carry a nanosecond
    /// timestamp suffix.
    pub fn track_prefix(
        mut self,
        table: &'static str,
        column: &'static str,
        prefix: impl Into<String>,
    ) -> Self {
        self.deletions
            .push((table, column, format!("{}%", prefix.into())));
        self
    }

    /// Track by exact name rather than prefix, for the (rarer) test that
    /// names a row without going through `unique_name`.
    pub fn track_exact(
        mut self,
        table: &'static str,
        column: &'static str,
        value: impl Into<String>,
    ) -> Self {
        self.deletions.push((table, column, value.into()));
        self
    }

    /// Track a `DELETE FROM <table> WHERE <column> LIKE '%<suffix>'`, for
    /// tests that append one shared unique suffix (e.g. a nanosecond
    /// timestamp) to every name they mint instead of a per-purpose prefix —
    /// one call here then covers every row the test created regardless of
    /// how many distinct tags it used.
    pub fn track_suffix(
        mut self,
        table: &'static str,
        column: &'static str,
        suffix: impl Into<String>,
    ) -> Self {
        self.deletions
            .push((table, column, format!("%{}", suffix.into())));
        self
    }
}

impl Default for TestCleanup {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestCleanup {
    fn drop(&mut self) {
        if self.deletions.is_empty() {
            return;
        }
        let deletions = std::mem::take(&mut self.deletions);
        // `Drop` cannot be `async`, and `block_in_place` panics unless the
        // enclosing runtime is multi-threaded (many of these `#[tokio::test]`
        // functions are not) — so run the cleanup on its own thread with its
        // own tiny runtime rather than assume anything about the caller's.
        //
        // Deliberately a *fresh* connection pool, opened from `DATABASE_URL`
        // inside that new runtime, rather than the test's own `PgPool`
        // cloned in: a `sqlx`/tokio connection's I/O is bound to the runtime
        // that drove it into existence, and reusing a pool created under the
        // test's runtime from this thread's different runtime made every
        // acquire here block until `sqlx`'s 30s pool-acquire timeout, fail,
        // and leave the row behind — the exact bug this module exists to
        // prevent, just moved one level down.
        let joined = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build cleanup runtime");
            rt.block_on(async {
                let Ok(url) = std::env::var("DATABASE_URL") else {
                    eprintln!("test cleanup: DATABASE_URL not set, skipping");
                    return;
                };
                let pool = match sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&url)
                    .await
                {
                    Ok(pool) => pool,
                    Err(err) => {
                        eprintln!("test cleanup: could not connect to clean up: {err}");
                        return;
                    }
                };
                for (table, column, pattern) in deletions {
                    let sql = format!("DELETE FROM {table} WHERE {column} LIKE $1");
                    if let Err(err) = sqlx::query(&sql).bind(&pattern).execute(&pool).await {
                        // Best-effort: a failed cleanup must not turn a
                        // passing test into a failing one, and there is no
                        // test outcome left to fail at this point anyway.
                        eprintln!("test cleanup: DELETE FROM {table} ({pattern}) failed: {err}");
                    }
                }
            });
        })
        .join();
        if joined.is_err() {
            eprintln!("test cleanup thread panicked");
        }
    }
}
