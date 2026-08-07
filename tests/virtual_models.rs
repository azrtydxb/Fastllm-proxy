//! End-to-end virtual-model routing against the real binary and a real
//! Postgres database, extending the `tests/rbac.rs` pattern from `File`
//! mode to `--role all` — virtual models are a control-plane feature (P1
//! depends on P0's database), so `File` mode has nowhere to store a rule at
//! all.
//!
//! Requires `DATABASE_URL` in the environment, same as every other
//! Postgres-backed test in this repo:
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) cargo test --features control -- --include-ignored
//! ```
//!
//! Setting up a fine-grained "this principal may invoke exactly this one
//! model" grant has no admin-API route (only whole *roles* are grantable,
//! and this repo seeds just `admin`/`operator`/`inference`), so this test
//! reaches into Postgres directly for that part, the same way
//! `control::build`'s and `control::api`'s own DB-backed tests build their
//! fixtures. Everything that *does* have a route — models, backends,
//! virtual models, rules, targets, principals, keys — goes through the
//! admin API's real HTTP surface, exactly as an operator would use it.

use std::io::Read;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[path = "support/mod.rs"]
mod support;
use support::TestCleanup;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        // Best-effort: runs during unwind too, so a failing assertion never
        // leaks a listener that would collide with the next run.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const PROXY_TOKEN: &str = "virtual-model-e2e-proxy-token";

/// A fixed, valid 32-byte key. What it decrypts is irrelevant here — this
/// test creates no encrypted `upstream_api_key`, and `build_snapshot`
/// already tolerates a row encrypted under a different key (see that
/// module's doc comment) by dropping just that one backend, not failing
/// startup.
fn encryption_key() -> String {
    "ff".repeat(32)
}

fn start(port: u16, admin_port: u16, database_url: &str) -> Proc {
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
        .spawn()
        .expect("failed to spawn fastllm-proxy --role all");
    let proc = Proc(child);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // Any HTTP answer proves the listener is up, including 503:
        // `/health` reports 503 when no backend is healthy, which is the
        // normal state for a control plane whose database has no models
        // yet. Treating only 2xx as "started" made these tests pass on a
        // database that happened to hold a model and hang for the full
        // timeout on an empty one — the server was listening and
        // answering the entire time.
        if !matches!(
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
            return proc;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy --role all on port {port} did not answer /health within 20s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn admin_post(
    admin_port: u16,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let resp = ureq::post(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .send_json(body.clone());
    match resp {
        Ok(r) => r.into_json().unwrap_or(serde_json::Value::Null),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            panic!("admin POST {path} with {body} failed: {code} {text}");
        }
        Err(e) => panic!("admin POST {path} failed: {e}"),
    }
}

fn admin_get(admin_port: u16, cookie: &str, path: &str) -> serde_json::Value {
    let resp = ureq::get(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .call();
    match resp {
        Ok(r) => r.into_json().unwrap(),
        Err(e) => panic!("admin GET {path} failed: {e}"),
    }
}

fn chat(port: u16, key: &str, model: &str) -> (u16, String) {
    let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"))
        .send_json(ureq::json!({ "model": model, "messages": [] }));
    match resp {
        Ok(r) => (r.status(), String::new()),
        Err(ureq::Error::Status(code, r)) => {
            let mut buf = String::new();
            let _ = r.into_reader().read_to_string(&mut buf);
            (code, buf)
        }
        Err(e) => panic!("request failed: {e}"),
    }
}

/// Grant `principal_id` `model:invoke` on exactly one model, through a
/// dedicated role named `role_name`. No admin route grants a single model
/// this precisely (only whole roles are grantable over HTTP), so this talks
/// to Postgres directly — see the module doc comment.
async fn grant_one_model(
    pool: &sqlx::PgPool,
    principal_id: i64,
    model_name: &str,
    role_name: &str,
) {
    sqlx::query("INSERT INTO roles (name) VALUES ($1) ON CONFLICT (name) DO NOTHING")
        .bind(role_name)
        .execute(pool)
        .await
        .unwrap();
    let role_id: i64 = sqlx::query_scalar("SELECT id FROM roles WHERE name = $1")
        .bind(role_name)
        .fetch_one(pool)
        .await
        .unwrap();
    let resource = format!("model/{model_name}");
    sqlx::query(
        "INSERT INTO permissions (verb, resource) VALUES ('model:invoke', $1)
         ON CONFLICT (verb, resource) DO NOTHING",
    )
    .bind(&resource)
    .execute(pool)
    .await
    .unwrap();
    let permission_id: i64 = sqlx::query_scalar(
        "SELECT id FROM permissions WHERE verb = 'model:invoke' AND resource = $1",
    )
    .bind(&resource)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(role_id)
    .bind(permission_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO principal_roles (principal_id, role_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(principal_id)
    .bind(role_id)
    .execute(pool)
    .await
    .unwrap();
}

/// A virtual model whose rule matches on the caller's role routes a
/// canary-role caller and an ordinary caller to two different concrete
/// models — proven by which backend's (unreachable) `api_base` shows up in
/// the resulting `502`'s error text, since that text is only ever produced
/// after `Router::pick` has committed to a specific backend in a specific
/// model's pool. This is what a unit test over `crate::routing` alone
/// cannot prove: that a real HTTP request, through the real admin API and
/// the real request path, ends up at the target the rule says it should.
///
/// Also pins the authorisation decision documented on
/// `proxy::resolve_target_model` at the full end-to-end level: a caller
/// granted only the *virtual* name, with no grant on the concrete model it
/// resolves to today, is refused — being granted a virtual model must never
/// by itself unlock whatever it happens to route to.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_virtual_models_rule_reaches_the_right_backend_and_authorisation_checks_the_resolved_model(
) {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();

    let port = 14421;
    let admin_port = 14422;
    let _p = start(port, admin_port, &database_url);

    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string();
    let name = |tag: &str| format!("{tag}-{suffix}");

    // Every name this test mints goes through `name()` above, so it always
    // ends in this one shared `suffix` regardless of tag — one cleanup
    // tracker per table covers all of it. A `Drop` guard rather than a
    // cleanup statement at the end of the test body: this test's own
    // `assert_eq!`s above panic on failure, and a line after them would
    // simply never run in that case, which is exactly how this shared
    // database accumulated hundreds of leftover rows before.
    let _cleanup = TestCleanup::new()
        .track_suffix("models", "name", suffix.clone())
        .track_suffix("virtual_models", "name", suffix.clone())
        .track_suffix("principals", "name", suffix.clone())
        .track_suffix("roles", "name", suffix.clone());

    // `/admin/*` is session-gated (P4): log in as a throwaway admin user
    // (also covered by the suffix-based cleanup above) before any
    // `admin_post`/`admin_get` call below.
    support::bootstrap_login_user(&pool, &name("vm-e2e-admin")).await;
    let cookie = support::login_cookie(admin_port, &name("vm-e2e-admin"));

    // Two concrete models, each with one backend at an address nothing is
    // listening on — the point is not a successful completion, it is
    // *which* backend's address ends up in the failure.
    let model_a_name = name("vm-e2e-backend-a");
    let model_a = admin_post(
        admin_port,
        &cookie,
        "/admin/models",
        serde_json::json!({ "name": model_a_name }),
    );
    let model_a_id = model_a["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/models/{model_a_id}/backends"),
        serde_json::json!({ "api_base": "http://127.0.0.1:19301/v1" }),
    );

    let model_b_name = name("vm-e2e-backend-b");
    let model_b = admin_post(
        admin_port,
        &cookie,
        "/admin/models",
        serde_json::json!({ "name": model_b_name }),
    );
    let model_b_id = model_b["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/models/{model_b_id}/backends"),
        serde_json::json!({ "api_base": "http://127.0.0.1:19302/v1" }),
    );

    // The virtual model: a rule that only matches the canary role, routing
    // to backend-b; everyone else falls through to the defaults, backend-a.
    let canary_role = name("vm-e2e-canary-role");
    let vm_name = name("vm-e2e-canary-vm");
    let vm = admin_post(
        admin_port,
        &cookie,
        "/admin/virtual-models",
        serde_json::json!({ "name": vm_name }),
    );
    let vm_id = vm["id"].as_i64().unwrap();

    let rule = admin_post(
        admin_port,
        &cookie,
        &format!("/admin/virtual-models/{vm_id}/rules"),
        serde_json::json!({ "position": 0, "roles": [canary_role] }),
    );
    let rule_id = rule["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/rules/{rule_id}/targets"),
        serde_json::json!({ "model_id": model_b_id, "weight": 100, "position": 0 }),
    );
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/virtual-models/{vm_id}/defaults"),
        serde_json::json!({ "model_id": model_a_id, "weight": 100, "position": 0 }),
    );

    // Three principals: one that matches the rule (and is granted the model
    // it resolves to), one that does not match it (granted the default's
    // model), and one granted only the virtual name itself.
    let canary_principal = admin_post(
        admin_port,
        &cookie,
        "/admin/principals",
        serde_json::json!({ "name": name("vm-e2e-canary-caller") }),
    );
    let canary_principal_id = canary_principal["id"].as_i64().unwrap();
    // One role both makes `roles.matches` true (the rule's own condition)
    // and grants `model:invoke` on the model that rule routes to.
    grant_one_model(&pool, canary_principal_id, &model_b_name, &canary_role).await;

    let normal_principal = admin_post(
        admin_port,
        &cookie,
        "/admin/principals",
        serde_json::json!({ "name": name("vm-e2e-normal-caller") }),
    );
    let normal_principal_id = normal_principal["id"].as_i64().unwrap();
    grant_one_model(
        &pool,
        normal_principal_id,
        &model_a_name,
        &name("vm-e2e-grant-a"),
    )
    .await;

    let virtual_only_principal = admin_post(
        admin_port,
        &cookie,
        "/admin/principals",
        serde_json::json!({ "name": name("vm-e2e-virtual-only-caller") }),
    );
    let virtual_only_principal_id = virtual_only_principal["id"].as_i64().unwrap();
    grant_one_model(
        &pool,
        virtual_only_principal_id,
        &vm_name,
        &name("vm-e2e-grant-vm"),
    )
    .await;

    let canary_key = admin_post(
        admin_port,
        &cookie,
        "/admin/keys",
        serde_json::json!({ "name": "canary-key", "principal_id": canary_principal_id }),
    )["key"]
        .as_str()
        .unwrap()
        .to_string();
    let normal_key = admin_post(
        admin_port,
        &cookie,
        "/admin/keys",
        serde_json::json!({ "name": "normal-key", "principal_id": normal_principal_id }),
    )["key"]
        .as_str()
        .unwrap()
        .to_string();
    let virtual_only_key = admin_post(
        admin_port,
        &cookie,
        "/admin/keys",
        serde_json::json!({ "name": "virtual-only-key", "principal_id": virtual_only_principal_id }),
    )["key"]
        .as_str()
        .unwrap()
        .to_string();

    // The keys were just minted; give the poll-free `--role all` write path
    // no excuse to be stale — `refresh()` runs synchronously inside every
    // admin POST, so by the time `admin_post` returned, the snapshot these
    // requests read was already current.

    let (status, body) = chat(port, &canary_key, &vm_name);
    assert_eq!(
        status, 502,
        "granted and routed to a real (if unreachable) backend: {body}"
    );
    assert!(
        body.contains("19302"),
        "the canary-role caller must reach backend-b's address: {body}"
    );

    let (status, body) = chat(port, &normal_key, &vm_name);
    assert_eq!(
        status, 502,
        "granted and routed to the default backend: {body}"
    );
    assert!(
        body.contains("19301"),
        "a caller with no matching rule must fall through to the default, backend-a: {body}"
    );

    // The authorisation decision, pinned end to end: this caller is granted
    // only the virtual model's *name*, never the concrete model ("backend-a")
    // it resolves to for a non-canary caller. If authorisation checked the
    // virtual name instead of the resolved target, this would incorrectly
    // succeed (reach routing, 502) instead of being refused.
    let (status, _) = chat(port, &virtual_only_key, &vm_name);
    assert_eq!(
        status, 403,
        "a grant on the virtual name alone must not unlock the concrete model it resolves to"
    );

    // An unknown model — virtual or concrete — is still a 404 regardless of
    // any of this.
    let (status, _) = chat(port, &canary_key, &name("vm-e2e-does-not-exist"));
    assert_eq!(status, 404);

    // `/v1/models` lists the virtual model alongside concrete ones.
    let resp = ureq::get(&format!("http://127.0.0.1:{port}/v1/models"))
        .set("authorization", &format!("Bearer {canary_key}"))
        .call()
        .unwrap();
    let listed: serde_json::Value = resp.into_json().unwrap();
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&vm_name.as_str()));
    assert!(ids.contains(&model_a_name.as_str()));
    assert!(ids.contains(&model_b_name.as_str()));

    // `GET /admin/virtual-models` reflects the same rule and targets that
    // were just exercised over the data plane.
    let admin_view = admin_get(admin_port, &cookie, "/admin/virtual-models");
    let vm_view = admin_view
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == vm_name)
        .unwrap();
    assert_eq!(vm_view["rules"].as_array().unwrap().len(), 1);
    assert_eq!(vm_view["rules"][0]["roles"][0], canary_role);
    assert_eq!(vm_view["default_targets"][0]["model"], model_a_name);

    // `_cleanup` (declared above, right after `pool`) deletes every model,
    // virtual model, principal and role this test created — including
    // `grant_one_model`'s roles, which no admin route creates — when it
    // drops at the end of this function, panic or not.
}
