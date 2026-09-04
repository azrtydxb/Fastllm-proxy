//! Cleanup helper for the `#[ignore = "requires postgres"]` tests in this
//! module.
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
