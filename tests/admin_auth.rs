//! P4: admin session authentication, end to end against a real `--role all`
//! process and a real Postgres database — the fix for the gap `TODO.md` has
//! documented since P0: "`/admin/*` has no authentication of its own".
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) cargo test --features control -- --include-ignored
//! ```

use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;
use support::TestCleanup;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const PROXY_TOKEN: &str = "admin-auth-e2e-proxy-token";

fn encryption_key() -> String {
    "ab".repeat(32)
}

fn start_all(port: u16, admin_port: u16, database_url: &str) -> Proc {
    // The child's stderr goes to a file rather than being inherited so that a
    // startup *failure* and mere startup *slowness* stop looking identical.
    // Without this, a process that exited immediately (a bad env var, a
    // migration error) and one that is simply still booting both surfaced as
    // the same bare "did not answer /health" timeout, which is unactionable
    // from a CI log.
    let log_path = std::env::temp_dir().join(format!("fastllm-admin-auth-{port}.log"));
    let log = std::fs::File::create(&log_path).expect("create child log");
    let child = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
        .args([
            "--role",
            "all",
            "--port",
            &port.to_string(),
            "--admin-port",
            &admin_port.to_string(),
        ])
        .env("FASTLLM_DATABASE_URL", database_url)
        .env("FASTLLM_PROXY_TOKEN", PROXY_TOKEN)
        .env("FASTLLM_ENCRYPTION_KEY", encryption_key())
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .expect("failed to spawn fastllm-proxy --role all");
    let mut proc = Proc(child);
    // Generous, because these tests run concurrently: each spawns its own
    // `--role all`, and they serialise behind one another on the migration
    // advisory lock against a shared database.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if ureq::get(&format!("http://127.0.0.1:{port}/health"))
            .call()
            .is_ok()
        {
            return proc;
        }
        if let Ok(Some(status)) = proc.0.try_wait() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fastllm-proxy --role all on port {port} exited early ({status}):\n{log}");
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "fastllm-proxy --role all on port {port} did not answer /health within 90s; \
                 child stderr:\n{log}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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

/// The cookie value out of a response's `Set-Cookie` header, stripped of
/// every attribute (`; HttpOnly; ...`) so it can be replayed verbatim as a
/// `Cookie` header on the next request — exactly what a browser does.
fn cookie_value(resp: &ureq::Response) -> String {
    let raw = resp.header("set-cookie").expect("login must set a cookie");
    let (name_value, _attrs) = raw.split_once(';').unwrap_or((raw, ""));
    name_value.trim().to_string()
}

fn admin_get_with_cookie(admin_port: u16, path: &str, cookie: Option<&str>) -> u16 {
    let mut req = ureq::get(&format!("http://127.0.0.1:{admin_port}{path}"));
    if let Some(c) = cookie {
        req = req.set("cookie", c);
    }
    match req.call() {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("GET {path} failed: {e}"),
    }
}

/// A request with no `Cookie` header at all is refused, a request with a
/// bogus one is refused the same way, a correct login sets a cookie that is
/// then accepted, and logout invalidates it — the whole session lifecycle
/// the design's "session cookie backed by Argon2id passwords" spec calls
/// for, pinned end to end rather than just at the unit level `control::auth`'s
/// own tests already cover.
#[tokio::test]
#[ignore = "requires postgres"]
async fn admin_routes_require_a_session_and_login_logout_manage_one() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let name = unique_name("admin-auth-user");
    // Deletes the bootstrapped user principal this test creates (and, by
    // cascade, its session rows) even if an assertion below panics.
    let _cleanup = TestCleanup::new().track_exact("principals", "name", name.clone());
    // A raw `INSERT` with a precomputed Argon2id hash, not
    // `control::auth::hash_password`/`bootstrap_admin_user` — this whole
    // file (like `budgets.rs`/`virtual_models.rs` before it) deliberately
    // never references `fastllm_proxy::control::*` directly, only `sqlx`,
    // so it still compiles under `cargo build --no-default-features` (which
    // has no `control` module at all) even though this particular test only
    // ever *runs* against a binary built with it. The hash below is
    // `hash_password("correct horse battery staple")`'s real output —
    // regenerate it with that function if the password below ever changes.
    sqlx::query("INSERT INTO principals (kind, name, password_hash) VALUES ('user', $1, $2)")
        .bind(&name)
        .bind("$argon2id$v=19$m=19456,t=2,p=1$ADQjtj0AshO4tt+y8yYwgA$b9DCigRtGIAy9y9xRa9Lip7A9pBHRU+4AMK9lJkXDOA")
        .execute(&pool)
        .await
        .unwrap();

    let port = 14721;
    let admin_port = 14722;
    let _p = start_all(port, admin_port, &database_url);

    // No cookie at all: refused.
    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/models", None),
        401,
        "an unauthenticated request to /admin/* must be refused"
    );

    // A cookie naming a session that does not exist: refused the same way.
    assert_eq!(
        admin_get_with_cookie(
            admin_port,
            "/admin/models",
            Some("fastllm_session=not-a-real-session-token")
        ),
        401,
        "a bogus session cookie must be refused, not treated as valid"
    );

    // Wrong password: login itself is refused, and no cookie is set.
    let wrong = ureq::post(&format!("http://127.0.0.1:{admin_port}/login"))
        .send_json(ureq::json!({ "name": name, "password": "not the password" }));
    match wrong {
        Err(ureq::Error::Status(401, r)) => {
            assert!(
                r.header("set-cookie").is_none(),
                "a rejected login must not set a session cookie"
            );
        }
        other => panic!("expected 401 for a wrong password, got {other:?}"),
    }

    // The right password: login succeeds and sets a cookie.
    let ok = ureq::post(&format!("http://127.0.0.1:{admin_port}/login"))
        .send_json(ureq::json!({ "name": name, "password": "correct horse battery staple" }))
        .expect("login with the correct password must succeed");
    assert_eq!(ok.status(), 200);
    let cookie = cookie_value(&ok);
    assert!(cookie.starts_with("fastllm_session="));

    // That cookie is now accepted on an /admin/* route.
    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/models", Some(&cookie)),
        200,
        "a valid session must be accepted"
    );

    // Logout invalidates it.
    let logout = ureq::post(&format!("http://127.0.0.1:{admin_port}/logout"))
        .set("cookie", &cookie)
        .call()
        .expect("logout must succeed");
    assert_eq!(logout.status(), 204);
    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/models", Some(&cookie)),
        401,
        "a session must be rejected once logged out"
    );
}

/// `PUT /admin/principals/{id}/password` is itself gated by `require_session`
/// like every other `/admin/*` route — it does not (and must not) offer a
/// second, unauthenticated way to set a password, or `require_session`'s
/// whole point would be moot.
// `ureq::Error` is a large enum and clippy's `result_large_err` fires on
// the `or_else` below. Boxing it in a test helper buys nothing, and CI's
// toolchain is newer than most local ones, so this has to be silenced
// explicitly rather than relying on the lint not firing locally.
#[allow(clippy::result_large_err)]
#[tokio::test]
#[ignore = "requires postgres"]
async fn setting_a_password_over_the_admin_api_requires_a_session_too() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let name = unique_name("admin-auth-victim");
    let _cleanup = TestCleanup::new().track_exact("principals", "name", name.clone());
    let principal_id: i64 =
        sqlx::query_scalar("INSERT INTO principals (kind, name) VALUES ('user', $1) RETURNING id")
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();

    let port = 14723;
    let admin_port = 14724;
    let _p = start_all(port, admin_port, &database_url);

    let resp = ureq::put(&format!(
        "http://127.0.0.1:{admin_port}/admin/principals/{principal_id}/password"
    ))
    .send_json(ureq::json!({ "password": "should-not-be-allowed" }));
    match resp {
        Err(ureq::Error::Status(401, _)) => {}
        other => panic!("expected 401 with no session, got {other:?}"),
    }
}

/// P4's role restriction, pinned end to end: the management UI is mounted
/// only on the admin API `--role all`/`control` serve (this test's
/// `admin_port`), never on the data-plane listener a `--role proxy` process
/// answers on. Deliberately tolerant of *which* UI response the admin port
/// gives (a real built `index.html` if `npm run build` has run in `web/`,
/// or `control::ui::serve_asset`'s 503 placeholder if `web/dist/` is still
/// empty, the normal state for a plain `cargo build`/`cargo test`) — the
/// property under test is that route existing at all, not its content, and
/// asserting one specific body would make this test depend on whether the
/// frontend happens to be built in whatever environment runs it.
// `ureq::Error` is a large enum and clippy's `result_large_err` fires on
// the `or_else` below. Boxing it in a test helper buys nothing, and CI's
// toolchain is newer than most local ones, so this has to be silenced
// explicitly rather than relying on the lint not firing locally.
#[allow(clippy::result_large_err)]
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_management_ui_route_exists_on_the_admin_port_and_not_on_the_proxy_port() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let port = 14725;
    let admin_port = 14726;
    let _p = start_all(port, admin_port, &database_url);

    // The admin port falls back to `control::ui::serve_asset` for an
    // unmatched path, and reachable with no session — the UI shell itself
    // (or, here, the "not available" placeholder) has to load before the
    // React app inside it can even attempt a login.
    let resp = ureq::get(&format!("http://127.0.0.1:{admin_port}/"))
        .call()
        .or_else(|e| match e {
            ureq::Error::Status(_, r) => Ok(r),
            e => Err(e),
        })
        .unwrap();
    let body = resp.into_string().unwrap();
    assert!(
        body.contains("management UI") || body.contains("<div id=\"root\">"),
        "the admin port must route `/` to the UI (built or its empty-dist placeholder): {body}"
    );

    // A plain `--role proxy` (`File` mode) process, same as `tests/rbac.rs`
    // spins up: no admin API, no `control::api::serve` call at all, so no
    // UI fallback route to have accidentally inherited.
    let proxy_config = "model_list:\n  - model_name: m\n    litellm_params: { api_base: http://127.0.0.1:8299/v1 }\n";
    let proxy_port = 14727;
    let path = std::env::temp_dir().join(format!("admin-auth-proxy-{proxy_port}.yaml"));
    std::fs::write(&path, proxy_config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
        .args([
            "--config",
            path.to_str().unwrap(),
            "--port",
            &proxy_port.to_string(),
            "--role",
            "proxy",
        ])
        .spawn()
        .expect("failed to spawn fastllm-proxy --role proxy");
    let _proxy = Proc(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if ureq::get(&format!("http://127.0.0.1:{proxy_port}/health"))
            .call()
            .is_ok()
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy --role proxy did not answer /health within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let proxy_resp = ureq::get(&format!("http://127.0.0.1:{proxy_port}/"))
        .call()
        .or_else(|e| match e {
            ureq::Error::Status(_, r) => Ok(r),
            e => Err(e),
        })
        .unwrap();
    let proxy_body = proxy_resp.into_string().unwrap_or_default();
    assert!(
        !proxy_body.contains("management UI"),
        "the proxy role must never serve the management UI: {proxy_body}"
    );
}
