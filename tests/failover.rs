//! Cross-model failover, and the conditions that decide where a request goes.
//!
//! Unit tests in `src/routing.rs` prove each condition in isolation. This file
//! proves the thing they structurally cannot: that a real request, through the
//! real binary and the real admin API, moves to a *different model* when the
//! first one refuses it.
//!
//! The motivating case is concrete and current — a hosted provider's free tier
//! answering 429. Health-based selection cannot help there: the pool is
//! perfectly healthy, it is simply refusing this request. Before the candidate
//! chain existed, that request had nowhere to go.
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) cargo test --features control -- --include-ignored
//! ```

use std::io::Read;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;
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

const PROXY_TOKEN: &str = "failover-e2e-proxy-token";

fn encryption_key() -> String {
    "ee".repeat(32)
}

/// See `tests/native_protocols.rs` for why the port is part of this: a
/// timestamp alone is not unique enough to keep parallel tests from colliding
/// on `principals_name_key`.
fn suffix(port: u16) -> String {
    format!(
        "{port}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn cleanup_for(suffix: &str) -> TestCleanup {
    TestCleanup::new()
        .track_suffix("models", "name", suffix)
        .track_suffix("virtual_models", "name", suffix)
        .track_suffix("principals", "name", suffix)
        .track_suffix("api_keys", "name", suffix)
        .track_suffix("roles", "name", suffix)
        .track_suffix("permissions", "resource", suffix)
}

/// Wait for both listeners `--role all` binds.
///
/// `/health` on the data plane says nothing about the admin API, which is
/// bound separately and later — and this suite drives the admin port to move
/// backends around. Waiting on only the proxy is a race that shows up as a
/// connection refused on the admin port, which is how it surfaced in CI.
fn wait_healthy(port: u16, admin_port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let proxy_up = !matches!(
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        );
        let admin_up = !matches!(
            ureq::get(&format!("http://127.0.0.1:{admin_port}/healthz")).call(),
            Err(ureq::Error::Transport(_))
        );
        if proxy_up && admin_up {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "fastllm-proxy did not answer both /health on {port} and /healthz on \
                 {admin_port} within 20s"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_all(port: u16, admin_port: u16, database_url: &str) -> Proc {
    let child = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
        .args([
            "--role",
            "all",
            "--port",
            &port.to_string(),
            "--admin-port",
            &admin_port.to_string(),
            "--snapshot-rebuild-interval",
            "1",
        ])
        .env("FASTLLM_DATABASE_URL", database_url)
        .env("FASTLLM_PROXY_TOKEN", PROXY_TOKEN)
        .env("FASTLLM_ENCRYPTION_KEY", encryption_key())
        // One pool per spawned process against a shared 100-connection
        // server; the default of 8 exhausts it once enough tests run at once.
        .env("FASTLLM_DATABASE_MAX_CONNECTIONS", "2")
        .spawn()
        .expect("failed to spawn fastllm-proxy --role all");
    let proc = Proc(child);
    wait_healthy(port, admin_port);
    proc
}

fn admin_post(
    admin_port: u16,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    match ureq::post(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .send_json(body.clone())
    {
        Ok(r) => r.into_json().unwrap_or(serde_json::Value::Null),
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            panic!("admin POST {path} with {body} failed: {code} {text}");
        }
        Err(e) => panic!("admin POST {path} failed: {e}"),
    }
}

fn admin_post_status(
    admin_port: u16,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> (u16, String) {
    match ureq::post(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .send_json(body)
    {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap_or_default()),
        Err(e) => panic!("admin POST {path} failed: {e}"),
    }
}

/// A mock upstream whose status code the test controls at runtime, so one
/// backend can start refusing mid-test without restarting anything.
#[derive(Clone)]
struct Upstream {
    status: Arc<AtomicU16>,
    hits: Arc<AtomicUsize>,
    name: &'static str,
}

impl Upstream {
    fn new(name: &'static str, status: u16) -> Self {
        Self {
            status: Arc::new(AtomicU16::new(status)),
            hits: Arc::new(AtomicUsize::new(0)),
            name,
        }
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }
    fn set_status(&self, status: u16) {
        self.status.store(status, Ordering::Relaxed);
    }
}

async fn spawn_upstream(port: u16, upstream: Upstream) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock upstream");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let upstream = upstream.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let upstream = upstream.clone();
                        async move {
                            use http_body_util::BodyExt;
                            let is_probe = req.method() == hyper::Method::GET;
                            let _ = req.into_body().collect().await;
                            // Probes always succeed: the point of these tests
                            // is failover driven by a *response status*, not by
                            // the health sweep, which could not see a 429 at
                            // all.
                            if is_probe {
                                return Ok::<_, std::convert::Infallible>(
                                    hyper::Response::builder()
                                        .status(200)
                                        .header("content-type", "application/json")
                                        .body(http_body_util::Full::new(bytes::Bytes::from(
                                            "{\"data\":[]}",
                                        )))
                                        .unwrap(),
                                );
                            }
                            upstream.hits.fetch_add(1, Ordering::Relaxed);
                            let status = upstream.status.load(Ordering::Relaxed);
                            let body = serde_json::json!({
                                "id": "cmpl-mock",
                                "object": "chat.completion",
                                "served_by": upstream.name,
                                "choices": [{"index": 0, "message": {"role": "assistant", "content": upstream.name}}],
                                "usage": {"prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10}
                            });
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder()
                                    .status(status)
                                    .header("content-type", "application/json")
                                    .body(http_body_util::Full::new(bytes::Bytes::from(
                                        body.to_string(),
                                    )))
                                    .unwrap(),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

fn chat(port: u16, key: &str, body: serde_json::Value) -> (u16, String) {
    match ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"))
        .send_json(body)
    {
        Ok(r) => {
            let status = r.status();
            let mut buf = String::new();
            let _ = r.into_reader().read_to_string(&mut buf);
            (status, buf)
        }
        Err(ureq::Error::Status(code, r)) => {
            let mut buf = String::new();
            let _ = r.into_reader().read_to_string(&mut buf);
            (code, buf)
        }
        Err(e) => panic!("chat request failed: {e}"),
    }
}

fn chat_with_header(
    port: u16,
    key: &str,
    header: Option<(&str, &str)>,
    body: serde_json::Value,
) -> (u16, String) {
    let mut req = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"));
    if let Some((name, value)) = header {
        req = req.set(name, value);
    }
    match req.send_json(body) {
        Ok(r) => {
            let status = r.status();
            let mut buf = String::new();
            let _ = r.into_reader().read_to_string(&mut buf);
            (status, buf)
        }
        Err(ureq::Error::Status(code, r)) => {
            let mut buf = String::new();
            let _ = r.into_reader().read_to_string(&mut buf);
            (code, buf)
        }
        Err(e) => panic!("chat request failed: {e}"),
    }
}

fn served_by(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["served_by"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("<unparseable: {body}>"))
}

/// Grant one principal `model:invoke` on a set of models, through a role made
/// for the purpose — no admin route is this fine-grained.
async fn grant_models(pool: &sqlx::PgPool, principal_id: i64, models: &[&str], role_name: &str) {
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
    for model in models {
        let resource = format!("model/{model}");
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
    }
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

struct Fixture {
    key: String,
    virtual_name: String,
    primary: String,
    secondary: String,
    vm_id: i64,
    cookie: String,
    admin_port: u16,
}

/// Two concrete models, each with one mock backend, plus a virtual model in
/// front of them and a principal granted both.
#[allow(clippy::too_many_arguments)]
async fn provision(
    pool: &sqlx::PgPool,
    admin_port: u16,
    cookie: &str,
    suffix: &str,
    primary_port: u16,
    secondary_port: u16,
) -> Fixture {
    let primary = format!("primary-{suffix}");
    let secondary = format!("secondary-{suffix}");
    let mut ids = Vec::new();
    for (name, port) in [(&primary, primary_port), (&secondary, secondary_port)] {
        let m = admin_post(
            admin_port,
            cookie,
            "/admin/models",
            serde_json::json!({"name": name, "description": "failover e2e"}),
        );
        let id = m["id"].as_i64().unwrap();
        admin_post(
            admin_port,
            cookie,
            &format!("/admin/models/{id}/backends"),
            serde_json::json!({
                "api_base": format!("http://127.0.0.1:{port}/v1"),
                "upstream_model": name,
            }),
        );
        ids.push(id);
    }

    let virtual_name = format!("vm-{suffix}");
    let vm = admin_post(
        admin_port,
        cookie,
        "/admin/virtual-models",
        serde_json::json!({"name": virtual_name}),
    );
    let vm_id = vm["id"].as_i64().unwrap();

    let principal = format!("fo-principal-{suffix}");
    let p = admin_post(
        admin_port,
        cookie,
        "/admin/principals",
        serde_json::json!({"name": principal}),
    );
    let principal_id = p["id"].as_i64().unwrap();
    grant_models(
        pool,
        principal_id,
        &[&primary, &secondary, &virtual_name],
        &format!("fo-role-{suffix}"),
    )
    .await;
    let key_name = format!("fo-key-{suffix}");
    let k = admin_post(
        admin_port,
        cookie,
        "/admin/keys",
        serde_json::json!({"principal_id": principal_id, "name": key_name}),
    );

    Fixture {
        key: k["key"].as_str().unwrap().to_string(),
        virtual_name,
        primary,
        secondary,
        vm_id,
        cookie: cookie.to_string(),
        admin_port,
    }
}

impl Fixture {
    fn model_id(&self, name: &str) -> i64 {
        let models = ureq::get(&format!(
            "http://127.0.0.1:{}/admin/models",
            self.admin_port
        ))
        .set("cookie", &self.cookie)
        .call()
        .unwrap()
        .into_json::<serde_json::Value>()
        .unwrap();
        models
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .expect("model exists")["id"]
            .as_i64()
            .unwrap()
    }

    /// A default chain: primary first (all the weight), secondary behind it.
    fn set_default_chain(&self) {
        for (i, name) in [&self.primary, &self.secondary].into_iter().enumerate() {
            admin_post(
                self.admin_port,
                &self.cookie,
                &format!("/admin/virtual-models/{}/defaults", self.vm_id),
                serde_json::json!({
                    "model_id": self.model_id(name),
                    // All weight on the primary so the head of the chain is not
                    // a coin flip: this test is about the *tail*.
                    "weight": if i == 0 { 100 } else { 0 },
                    "position": i as i32,
                }),
            );
        }
    }
}

fn wait_for_virtual_model(port: u16, key: &str, model: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, body) = chat(
            port,
            key,
            serde_json::json!({"model": model, "messages": [{"role": "user", "content": "ping"}]}),
        );
        if status != 404 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("virtual model {model} never reached the snapshot: {status} {body}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The headline behaviour: the first model in the chain answers 429, and the
/// request lands on the second rather than reaching the client as an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_429_from_the_first_model_fails_over_to_the_next_in_the_chain() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, p1, p2) = (14811, 14812, 14813, 14814);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let primary_up = Upstream::new("primary", 200);
    let secondary_up = Upstream::new("secondary", 200);
    spawn_upstream(p1, primary_up.clone()).await;
    spawn_upstream(p2, secondary_up.clone()).await;

    let admin_name = format!("fo-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let fx = provision(&pool, admin_port, &cookie, &suffix, p1, p2).await;
    fx.set_default_chain();
    wait_for_virtual_model(port, &fx.key, &fx.virtual_name);

    let req = serde_json::json!({
        "model": fx.virtual_name,
        "messages": [{"role": "user", "content": "hello"}],
    });

    // Healthy: the head of the chain serves.
    let (status, body) = chat(port, &fx.key, req.clone());
    assert_eq!(status, 200, "{body}");
    assert_eq!(served_by(&body), "primary");

    // Now the primary rate-limits. It is still *healthy* — the probe succeeds
    // — so nothing but a response-status-driven failover can save this request.
    primary_up.set_status(429);
    let before = secondary_up.hits();
    let (status, body) = chat(port, &fx.key, req.clone());
    assert_eq!(
        status, 200,
        "a 429 from the first model must fail over, not reach the client: {body}"
    );
    assert_eq!(served_by(&body), "secondary");
    assert!(
        secondary_up.hits() > before,
        "the second model should have been called"
    );

    // With nowhere left to go, the upstream's own status reaches the client
    // rather than a synthetic 502 — the same last-resort rule that already
    // governs a single-backend pool.
    secondary_up.set_status(429);
    let (status, _) = chat(port, &fx.key, req);
    assert_eq!(
        status, 429,
        "once the chain is exhausted the real upstream status is forwarded"
    );
}

/// Failover must never widen reach: a caller not granted the fallback model
/// gets the refusal, not a silent hop onto a model they cannot use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn failover_only_reaches_models_the_caller_was_already_granted() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, p1, p2) = (14821, 14822, 14823, 14824);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let primary_up = Upstream::new("primary", 429);
    let secondary_up = Upstream::new("secondary", 200);
    spawn_upstream(p1, primary_up.clone()).await;
    spawn_upstream(p2, secondary_up.clone()).await;

    let admin_name = format!("fo-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let fx = provision(&pool, admin_port, &cookie, &suffix, p1, p2).await;
    fx.set_default_chain();
    wait_for_virtual_model(port, &fx.key, &fx.virtual_name);

    // A second principal granted only the *primary* and the virtual name.
    let narrow_name = format!("fo-narrow-{suffix}");
    let narrow = admin_post(
        admin_port,
        &cookie,
        "/admin/principals",
        serde_json::json!({"name": narrow_name}),
    );
    let narrow_id = narrow["id"].as_i64().unwrap();
    grant_models(
        &pool,
        narrow_id,
        &[&fx.primary, &fx.virtual_name],
        &format!("fo-narrow-role-{suffix}"),
    )
    .await;
    let narrow_key = admin_post(
        admin_port,
        &cookie,
        "/admin/keys",
        serde_json::json!({"principal_id": narrow_id, "name": format!("fo-narrow-key-{suffix}")}),
    )["key"]
        .as_str()
        .unwrap()
        .to_string();

    // Give the snapshot time to carry the new grant.
    std::thread::sleep(Duration::from_secs(3));

    let req = serde_json::json!({
        "model": fx.virtual_name,
        "messages": [{"role": "user", "content": "hello"}],
    });

    // The broad principal fails over happily.
    let (status, body) = chat(port, &fx.key, req.clone());
    assert_eq!(status, 200, "{body}");
    assert_eq!(served_by(&body), "secondary");

    // The narrow one has no permitted fallback, so it gets the primary's own
    // 429 rather than being quietly routed onto a model it was never granted.
    let (status, body) = chat(port, &narrow_key, req);
    assert_eq!(
        status, 429,
        "a caller without a grant on the fallback must not be routed to it: {body}"
    );
}

/// Header-driven tiering, end to end: the client says what kind of work this
/// is, and the rule chain honours it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_header_rule_routes_batch_work_to_a_different_model() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, p1, p2) = (14831, 14832, 14833, 14834);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let primary_up = Upstream::new("primary", 200);
    let secondary_up = Upstream::new("secondary", 200);
    spawn_upstream(p1, primary_up.clone()).await;
    spawn_upstream(p2, secondary_up.clone()).await;

    let admin_name = format!("fo-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let fx = provision(&pool, admin_port, &cookie, &suffix, p1, p2).await;

    // Rule 0: batch-labelled work goes to the secondary. Default: primary.
    let rule = admin_post(
        admin_port,
        &cookie,
        &format!("/admin/virtual-models/{}/rules", fx.vm_id),
        serde_json::json!({"position": 0, "headers": {"x-fastllm-tier": "batch"}}),
    );
    let rule_id = rule["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/rules/{rule_id}/targets"),
        serde_json::json!({
            "model_id": fx.model_id(&fx.secondary), "weight": 100, "position": 0
        }),
    );
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/virtual-models/{}/defaults", fx.vm_id),
        serde_json::json!({"model_id": fx.model_id(&fx.primary), "weight": 100, "position": 0}),
    );
    wait_for_virtual_model(port, &fx.key, &fx.virtual_name);

    let req = serde_json::json!({
        "model": fx.virtual_name,
        "messages": [{"role": "user", "content": "hello"}],
    });

    let (status, body) = chat_with_header(
        port,
        &fx.key,
        Some(("x-fastllm-tier", "batch")),
        req.clone(),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(served_by(&body), "secondary", "labelled batch");

    let (status, body) = chat_with_header(port, &fx.key, None, req.clone());
    assert_eq!(status, 200, "{body}");
    assert_eq!(served_by(&body), "primary", "no label, so the default");

    let (status, body) =
        chat_with_header(port, &fx.key, Some(("x-fastllm-tier", "interactive")), req);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        served_by(&body),
        "primary",
        "a different value must not match"
    );
}

/// A malformed condition is refused when it is written, not silently stored as
/// a rule that never matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires postgres"]
async fn a_malformed_rule_condition_is_rejected_by_the_admin_api() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port) = (14841, 14842);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let admin_name = format!("fo-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    let vm = admin_post(
        admin_port,
        &cookie,
        "/admin/virtual-models",
        serde_json::json!({"name": format!("vm-{suffix}")}),
    );
    let vm_id = vm["id"].as_i64().unwrap();

    for (body, expect) in [
        (
            serde_json::json!({"position": 0, "after": "25:00"}),
            "after",
        ),
        (serde_json::json!({"position": 0, "days": [8]}), "days"),
        (
            serde_json::json!({"position": 0, "min_budget_used_percent": 200}),
            "min_budget_used_percent",
        ),
    ] {
        let (status, text) = admin_post_status(
            admin_port,
            &cookie,
            &format!("/admin/virtual-models/{vm_id}/rules"),
            body.clone(),
        );
        assert_eq!(status, 400, "{body} should be refused, got {text}");
        assert!(
            text.contains(expect),
            "the refusal must name the field: {text}"
        );
    }

    // And a well-formed one is still accepted.
    let (status, text) = admin_post_status(
        admin_port,
        &cookie,
        &format!("/admin/virtual-models/{vm_id}/rules"),
        serde_json::json!({
            "position": 0, "after": "22:00", "before": "06:00", "days": [1, 2, 3, 4, 5],
            "utc_offset_minutes": 120, "stream": false, "max_inflight_per_backend": 4
        }),
    );
    assert_eq!(status, 201, "{text}");
}

/// The deployment-wide last resort.
///
/// Rule-level failover only reaches targets that rule named. A chain whose
/// every model is unreachable has nowhere left to go — and a rule author cannot
/// anticipate every way that happens. The fallback catches those, and it
/// applies to a plain concrete model name too, not only a virtual one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_fallback_model_catches_a_chain_that_ran_out() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, p1, p2) = (14851, 14852, 14853, 14854);
    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    // The primary is dead: nothing is listening on p1 at all.
    let rescue = Upstream::new("rescue", 200);
    spawn_upstream(p2, rescue.clone()).await;

    let admin_name = format!("fo-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let fx = provision(&pool, admin_port, &cookie, &suffix, p1, p2).await;

    // No chain at all: the virtual model's only target is the dead primary.
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/virtual-models/{}/defaults", fx.vm_id),
        serde_json::json!({
            "model_id": fx.model_id(&fx.primary), "weight": 100, "position": 0
        }),
    );
    wait_for_virtual_model(port, &fx.key, &fx.virtual_name);

    let req = serde_json::json!({
        "model": fx.virtual_name,
        "messages": [{"role": "user", "content": "hello"}],
    });

    // Without a fallback the request has nowhere to go.
    let (status, _) = chat(port, &fx.key, req.clone());
    assert_eq!(status, 502, "a dead single-target chain should fail");

    // Name the second model as the deployment-wide fallback.
    let rescue_id = fx.model_id(&fx.secondary);
    ureq::put(&format!(
        "http://127.0.0.1:{admin_port}/admin/fallback-model"
    ))
    .set("cookie", &cookie)
    .send_json(serde_json::json!({"model_id": rescue_id}))
    .expect("setting the fallback model");
    std::thread::sleep(Duration::from_secs(3));

    let before = rescue.hits();
    let (status, body) = chat(port, &fx.key, req);
    assert_eq!(status, 200, "the fallback should have served it: {body}");
    assert_eq!(served_by(&body), "rescue");
    assert!(rescue.hits() > before);

    // It applies to a concrete model name too, not only a virtual one.
    let (status, body) = chat(
        port,
        &fx.key,
        serde_json::json!({
            "model": fx.primary,
            "messages": [{"role": "user", "content": "hello"}],
        }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        served_by(&body),
        "rescue",
        "a dead concrete model should also reach the fallback"
    );

    // Clearing it restores the previous behaviour, so this is not a one-way door.
    ureq::put(&format!(
        "http://127.0.0.1:{admin_port}/admin/fallback-model"
    ))
    .set("cookie", &cookie)
    .send_json(serde_json::json!({"model_id": serde_json::Value::Null}))
    .expect("clearing the fallback model");
    std::thread::sleep(Duration::from_secs(3));
    let (status, _) = chat(
        port,
        &fx.key,
        serde_json::json!({
            "model": fx.primary,
            "messages": [{"role": "user", "content": "hello"}],
        }),
    );
    assert_eq!(status, 502, "cleared means cleared");
}
