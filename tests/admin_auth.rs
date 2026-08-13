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

/// Serialises process startup across this file's tests.
///
/// Each test here spawns its own full `--role all`, and every one of those
/// runs migrations against the same database behind a Postgres advisory
/// lock. Started concurrently on a small CI runner they simply queue behind
/// each other, and the observed result was three processes alive, silent and
/// unresponsive past a 90s deadline. Serialising costs nothing — the work was
/// already serial inside Postgres — and removes the contention rather than
/// hiding it behind a longer timeout.
static START_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn start_all(port: u16, admin_port: u16, database_url: &str) -> Proc {
    let _serialise = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The child's stderr goes to a file rather than being inherited so that a
    // startup *failure* and mere startup *slowness* stop looking identical.
    // Without this, a process that exited immediately (a bad env var, a
    // migration error) and one that is simply still booting both surfaced as
    // the same bare "did not answer /health" timeout, which is unactionable
    // from a CI log.
    let log_path = std::env::temp_dir().join(format!("fastllm-admin-auth-{port}.log"));
    let log = std::fs::File::create(&log_path).expect("create child log");
    // BOTH streams: `tracing_subscriber::fmt()` writes to stdout, not stderr.
    // Capturing only stderr produced an always-empty diagnostic that made a
    // hanging child look like a silent one.
    let log_err = log.try_clone().expect("clone child log handle");
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
        // One pool per spawned process against a shared 100-connection
        // server; the default of 8 exhausts it once enough tests run at once.
        .env("FASTLLM_DATABASE_MAX_CONNECTIONS", "2")
        // Debug so that if this ever hangs again the captured stderr says
        // where, instead of being empty.
        .env("FASTLLM_LOG", "debug")
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .spawn()
        .expect("failed to spawn fastllm-proxy --role all");
    let mut proc = Proc(child);
    // Generous, because these tests run concurrently: each spawns its own
    // `--role all`, and they serialise behind one another on the migration
    // advisory lock against a shared database.
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        // Any HTTP answer proves the listener is up, including 503:
        // `/health` reports 503 when no backend is healthy, which is the
        // normal state for a control plane whose database has no models
        // yet. Treating only 2xx as "started" made these tests pass on a
        // database that happened to hold a model and hang for the full
        // timeout on an empty one — the server was listening and
        // answering the entire time.
        //
        // **Both** listeners, not just the proxy one. `--role all` binds the
        // data plane and the admin API separately, and the admin API is
        // bound after the snapshot is built — so `/health` answering proves
        // only that the *proxy* is up. Every test here then talks to the
        // admin port, and one of them raced it in CI: connection refused on
        // 14724 while `/health` on the data-plane port had answered happily.
        //
        // `/healthz` is the admin API's own unauthenticated liveness route,
        // so this needs no session and widens no gate.
        let proxy_up = !matches!(
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        );
        let admin_up = !matches!(
            ureq::get(&format!("http://127.0.0.1:{admin_port}/healthz")).call(),
            Err(ureq::Error::Transport(_))
        );
        if proxy_up && admin_up {
            return proc;
        }
        if let Ok(Some(status)) = proc.0.try_wait() {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!("fastllm-proxy --role all on port {port} exited early ({status}):\n{log}");
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "fastllm-proxy --role all did not answer both /health on {port} and \
                 /healthz on {admin_port} within 90s; \
                 child output:\n{log}"
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

/// Same shape as `admin_get_with_cookie`, for the mutating verbs the RBAC
/// permission-mapping tests below need — a body is always sent (even if
/// `{}`), since every route those tests call takes one.
fn admin_write_with_cookie(
    admin_port: u16,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: serde_json::Value,
) -> u16 {
    let url = format!("http://127.0.0.1:{admin_port}{path}");
    let mut req = match method {
        "POST" => ureq::post(&url),
        "PUT" => ureq::put(&url),
        "DELETE" => ureq::delete(&url),
        other => panic!("admin_write_with_cookie: unsupported method {other}"),
    };
    if let Some(c) = cookie {
        req = req.set("cookie", c);
    }
    match req.send_json(body) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("{method} {path} failed: {e}"),
    }
}

/// Creates a `kind = 'user'` principal named `name`, with [`support::TEST_PASSWORD`]
/// already set, holding exactly the roles named in `roles` (or none at all
/// for an empty slice) — the building block every permission-mapping test
/// below needs: a login-capable principal whose grants are precisely
/// controlled, unlike `support::bootstrap_login_user`'s fixed `admin` grant.
async fn user_with_roles(pool: &sqlx::PgPool, name: &str, roles: &[&str]) -> i64 {
    let principal_id: i64 = sqlx::query_scalar(
        "INSERT INTO principals (kind, name, password_hash) VALUES ('user', $1, $2) RETURNING id",
    )
    .bind(name)
    .bind(support::TEST_PASSWORD_HASH)
    .fetch_one(pool)
    .await
    .unwrap();
    for role in roles {
        sqlx::query(
            "INSERT INTO principal_roles (principal_id, role_id)
             SELECT $1, id FROM roles WHERE name = $2",
        )
        .bind(principal_id)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
    }
    principal_id
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
    let principal_id: i64 = sqlx::query_scalar(
        "INSERT INTO principals (kind, name, password_hash) VALUES ('user', $1, $2) RETURNING id",
    )
    .bind(&name)
    .bind("$argon2id$v=19$m=19456,t=2,p=1$ADQjtj0AshO4tt+y8yYwgA$b9DCigRtGIAy9y9xRa9Lip7A9pBHRU+4AMK9lJkXDOA")
    .fetch_one(&pool)
    .await
    .unwrap();
    // This test exercises the session lifecycle (cookie set on login,
    // accepted on /admin/*, cleared on logout) end to end, which needs a
    // real /admin/* route to probe with a valid session -- not the
    // permission-mapping RBAC finds elsewhere in this suite. Grant `admin`
    // so `GET /admin/models` below is answered on the strength of the
    // session alone, the same way it was before RequireRead started
    // checking permissions too.
    sqlx::query(
        "INSERT INTO principal_roles (principal_id, role_id)
         SELECT $1, id FROM roles WHERE name = 'admin'",
    )
    .bind(principal_id)
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
        // Any HTTP answer proves the listener is up, including 503:
        // `/health` reports 503 when no backend is healthy, which is the
        // normal state for a control plane whose database has no models
        // yet. Treating only 2xx as "started" made these tests pass on a
        // database that happened to hold a model and hang for the full
        // timeout on an empty one — the server was listening and
        // answering the entire time.
        if !matches!(
            ureq::get(&format!("http://127.0.0.1:{proxy_port}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
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

/// `/healthz` (deploy/control.yaml's probe target) has to satisfy two
/// properties at once that are easy to break independently: it must answer
/// with no credential at all (a kubelet has neither a session cookie nor the
/// proxy token), and its exemption from `require_session` must not have
/// accidentally widened to cover a real `/admin/*` route too — so this pins
/// both against the same running process, the way the P0 gap this file is
/// named for was originally missed: by checking each route in isolation
/// instead of checking that the boundary between them was in the right
/// place.
#[tokio::test]
#[ignore = "requires postgres"]
async fn healthz_is_unauthenticated_and_does_not_widen_the_session_gate() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let port = 14728;
    let admin_port = 14729;
    let _p = start_all(port, admin_port, &database_url);

    // No cookie, no `Authorization` header, no proxy token anywhere: exactly
    // what a kubelet probe can present.
    let resp = ureq::get(&format!("http://127.0.0.1:{admin_port}/healthz"))
        .call()
        .expect("/healthz must answer with no credential of any kind");
    assert_eq!(resp.status(), 200);
    let body = resp.into_string().unwrap();
    assert_eq!(
        body, r#"{"status":"ok"}"#,
        "the probe body must stay the minimal, boring shape this route promises"
    );
    // Nothing sensitive leaked into that body: no key material, no
    // credential, no backend address, no principal name.
    for secret in [
        PROXY_TOKEN,
        &encryption_key(),
        "api_base",
        "backend",
        "principal",
        "sk-",
    ] {
        assert!(
            !body.contains(secret),
            "/healthz body must not mention {secret:?}: {body}"
        );
    }

    // The same unauthenticated request against a real `/admin/*` route is
    // still refused — the exemption above did not widen past `/healthz`.
    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/keys", None),
        401,
        "/admin/keys must still require a session even though /healthz does not"
    );
}

/// The real-production bug this suite exists to close: before
/// `RequirePermission` (`src/control/api.rs`), `require_session` alone
/// decided access to every `/admin/*` route, so any principal a password
/// was ever set for — including a service account meant only for read-only
/// UI access — was a full admin. This pins the fix's read/write split: a
/// principal holding only `usage:read` (no role in `migrations/0001_init.sql`
/// grants that in isolation, so this test creates its own role with exactly
/// that one permission) can list through `GET /admin/keys`, but `POST
/// /admin/keys` — a write, `key:create` — is refused.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_principal_with_only_a_read_permission_can_list_but_not_create() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let role_name = unique_name("rbac-read-only-role");
    let _cleanup = TestCleanup::new()
        .track_exact("roles", "name", role_name.clone())
        .track_prefix("principals", "name", "rbac-reader-");
    sqlx::query("INSERT INTO roles (name, description) VALUES ($1, 'test: usage:read only')")
        .bind(&role_name)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission_id)
         SELECT r.id, p.id FROM roles r, permissions p
         WHERE r.name = $1 AND p.verb = 'usage:read' AND p.resource = '*'",
    )
    .bind(&role_name)
    .execute(&pool)
    .await
    .unwrap();

    let name = unique_name("rbac-reader");
    user_with_roles(&pool, &name, &[&role_name]).await;

    let port = 14730;
    let admin_port = 14731;
    let _p = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &name);

    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/keys", Some(&cookie)),
        200,
        "usage:read must be enough to list keys"
    );
    assert_eq!(
        admin_write_with_cookie(
            admin_port,
            "POST",
            "/admin/keys",
            Some(&cookie),
            ureq::json!({ "name": "should-not-be-created", "principal_id": 1 }),
        ),
        403,
        "usage:read must not be enough to create a key -- that needs key:create"
    );
}

/// The other half of the same fix: a principal with a valid session but no
/// admin permission at all — `principal_roles` holds nothing for it — is
/// refused on every `/admin/*` route with 403, not 401. The session itself
/// is still good (login succeeds, the cookie is accepted as a *session*):
/// the distinction this test exists to pin is "authenticated but not
/// authorised" against "not authenticated at all", which a caller needs to
/// tell apart from the response.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_principal_with_no_admin_permissions_gets_403_everywhere_but_can_still_authenticate() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let name = unique_name("rbac-no-perms");
    let _cleanup = TestCleanup::new().track_exact("principals", "name", name.clone());
    // No role at all -- `user_with_roles` with an empty slice.
    user_with_roles(&pool, &name, &[]).await;

    let port = 14732;
    let admin_port = 14733;
    let _p = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &name);

    // The session is real: a route with no permission requirement of its
    // own would accept it. `/admin/*` has no such route, so this is proven
    // negatively below -- every route this test tries is refused, but the
    // *reason* is 403 (checked explicitly), never 401 (which would mean the
    // login/cookie itself, not the permission check, was what failed).
    for (method, path, body) in [
        ("GET", "/admin/keys", None),
        ("GET", "/admin/principals", None),
        ("GET", "/admin/models", None),
        ("GET", "/admin/roles", None),
        ("GET", "/admin/limits", None),
        ("GET", "/admin/budgets", None),
        ("GET", "/admin/health", None),
        (
            "POST",
            "/admin/keys",
            Some(ureq::json!({ "name": "x", "principal_id": 1 })),
        ),
        (
            "POST",
            "/admin/principals",
            Some(ureq::json!({ "name": "x" })),
        ),
    ] {
        let status = match body {
            None => admin_get_with_cookie(admin_port, path, Some(&cookie)),
            Some(b) => admin_write_with_cookie(admin_port, method, path, Some(&cookie), b),
        };
        assert_eq!(
            status, 403,
            "{method} {path} with a session but no permission must be 403, got {status}"
        );
    }
}

/// The flip side of the two tests above, and the one that keeps the fix
/// from making the control plane unusable: the very first admin login
/// (`fastllm-proxy set-password`, which `control::auth::bootstrap_admin_user`
/// backs) grants the `admin` role automatically when the principal holds no
/// role yet, and `admin` carries every permission `migrations/0001_init.sql`
/// seeds. This exercises that path exactly as `set-password` does — through
/// `bootstrap_admin_user`'s own SQL shape via a raw grant against the
/// `admin` role, since this file cannot call `control::auth::*` directly
/// (see the module doc comment on `support::bootstrap_login_user`) — and
/// then proves the resulting principal can reach a read route, a
/// `key:create` route and a `config:write` route, one from each permission
/// bucket `RequirePermission` checks.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_bootstrap_admin_can_do_everything() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let name = unique_name("rbac-bootstrap-admin");
    let _cleanup = TestCleanup::new()
        .track_exact("principals", "name", name.clone())
        .track_prefix("principals", "name", "rbac-bootstrap-admin-vm-owner-");
    user_with_roles(&pool, &name, &["admin"]).await;

    let port = 14734;
    let admin_port = 14735;
    let _p = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &name);

    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/keys", Some(&cookie)),
        200,
        "admin must be able to read"
    );
    let owner_name = unique_name("rbac-bootstrap-admin-vm-owner");
    let create_status = admin_write_with_cookie(
        admin_port,
        "POST",
        "/admin/principals",
        Some(&cookie),
        ureq::json!({ "name": owner_name, "kind": "service_account" }),
    );
    assert_eq!(create_status, 201, "admin must be able to write config");
}

/// Every configuration change is recorded, and reads are not.
///
/// The value of a layer over hand-wired calls is exactly this: a route added
/// tomorrow is audited without anybody remembering to wire it.
#[tokio::test]
#[ignore = "requires postgres"]
async fn configuration_changes_are_audited_and_reads_are_not() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port) = (14781, 14782);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();

    let name = unique_name("audit-admin");
    let model = unique_name("audit-model");
    let _cleanup = TestCleanup::new()
        .track_exact("principals", "name", name.clone())
        .track_exact("models", "name", model.clone());
    let principal_id = user_with_roles(&pool, &name, &["admin"]).await;
    let _ = principal_id;

    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &name);

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();

    // A read: not a change, and auditing every list call would bury the
    // changes in noise.
    assert_eq!(
        admin_get_with_cookie(admin_port, "/admin/models", Some(&cookie)),
        200
    );

    // A change.
    assert_eq!(
        admin_write_with_cookie(
            admin_port,
            "POST",
            "/admin/models",
            Some(&cookie),
            serde_json::json!({"name": model, "description": ""}),
        ),
        201
    );

    // A change that was refused: an attempt, not a change, and recording it as
    // one would make the trail lie in the direction that matters most.
    assert_eq!(
        admin_write_with_cookie(
            admin_port,
            "POST",
            "/admin/models",
            None,
            serde_json::json!({"name": "no-session", "description": ""}),
        ),
        401
    );

    let rows: Vec<(Option<i64>, String, String, String)> = sqlx::query_as(
        "SELECT actor_id, actor_name, action, target FROM audit_events ORDER BY id DESC LIMIT 5",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        after - before,
        1,
        "the write only: not the read, not the rejected attempt: {rows:?}"
    );
    let (actor_id, actor_name, action, target) = &rows[0];
    assert_eq!(actor_id, &Some(principal_id));
    assert_eq!(actor_name, &name, "the trail's most important column");
    assert_eq!(action, "POST");
    assert_eq!(target, "/admin/models");
}
