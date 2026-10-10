//! Admission control, set per backend through the admin API, against a real
//! control plane, a real proxy and an engine that is drowning.
//!
//! `src/admission.rs` proves the gate's arithmetic in isolation. This proves
//! the path an operator actually uses: a gate set with
//! `PATCH /admin/backends/{id}` -- what the Models page sends -- reaches a
//! running proxy without a restart, shrinks when the engine reports a deep
//! queue, holds the surplus so the engine sees fewer concurrent requests than
//! were sent, refuses what it cannot hold with a 503 and `Retry-After`, and
//! shows all of it in `/metrics` and `/admin/fleet`.
//!
//! Each case that claims the gate bounded the engine has a twin with the gate
//! off, against the same fake engine and the same burst. Without it, "the
//! engine saw at most one request" could be the burst arriving slowly rather
//! than the gate doing anything.
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) cargo test --features control --test admission -- --include-ignored
//! ```

use std::io::Read;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

const PROXY_TOKEN: &str = "admission-e2e-proxy-token";

/// See `tests/native_protocols.rs` for why the port is part of this.
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
        .track_suffix("provider_models", "name", suffix)
        .track_suffix("principals", "name", suffix)
        .track_suffix("api_keys", "name", suffix)
        .track_suffix("roles", "name", suffix)
        .track_suffix("permissions", "resource", suffix)
}

/// A fake engine: what it reports about itself, and what it observed.
#[derive(Default)]
struct Engine {
    /// The queue depth it claims in `/metrics`. A fixture, not a count.
    waiting: u32,
    /// Chat requests in progress right now.
    running: AtomicUsize,
    /// The most that were ever in progress at once. The number under test.
    peak: AtomicUsize,
    /// Scrapes served. Doubles as the token counters it reports, so they
    /// climb on every read the way a busy engine's do -- frozen counters with
    /// `running > 0` is what the stall detector ejects a backend for, and an
    /// ejected backend would fail these tests for the wrong reason.
    scrapes: AtomicU64,
}

impl Engine {
    fn reporting(waiting: u32) -> Arc<Self> {
        Arc::new(Self {
            waiting,
            ..Default::default()
        })
    }
}

async fn spawn_engine(port: u16, engine: Arc<Engine>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind fake engine");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let engine = Arc::clone(&engine);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let engine = Arc::clone(&engine);
                        async move {
                            use http_body_util::BodyExt;
                            let path = req.uri().path().to_owned();
                            let _ = req.into_body().collect().await;

                            let (content_type, body) = match path.as_str() {
                                "/metrics" => {
                                    let n = engine.scrapes.fetch_add(1, Ordering::SeqCst) + 1;
                                    (
                                        "text/plain",
                                        format!(
                                            "vllm:num_requests_running{{engine=\"0\"}} 1.0\n\
                                             vllm:num_requests_waiting{{engine=\"0\"}} {}.0\n\
                                             vllm:prompt_tokens_total{{engine=\"0\"}} {n}.0\n\
                                             vllm:generation_tokens_total{{engine=\"0\"}} {n}.0\n",
                                            engine.waiting
                                        ),
                                    )
                                }
                                "/v1/models" => (
                                    "application/json",
                                    r#"{"object":"list","data":[{"id":"slow","object":"model"}]}"#
                                        .to_owned(),
                                ),
                                _ => {
                                    let now = engine.running.fetch_add(1, Ordering::SeqCst) + 1;
                                    engine.peak.fetch_max(now, Ordering::SeqCst);
                                    // Long enough that the whole burst is in
                                    // flight at once if nothing stops it.
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                    engine.running.fetch_sub(1, Ordering::SeqCst);
                                    (
                                        "application/json",
                                        serde_json::json!({
                                            "id": "cmpl-slow",
                                            "object": "chat.completion",
                                            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}],
                                            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                                        })
                                        .to_string(),
                                    )
                                }
                            };
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder()
                                    .status(200)
                                    .header("content-type", content_type)
                                    .body(http_body_util::Full::new(bytes::Bytes::from(body)))
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
            // Fast, so a gate exists and has reacted to the engine's queue
            // well inside the waits below.
            "--engine-scrape-interval",
            "1",
            "--health-report-interval",
            "1",
            // Strict rotation, so which backend a request tries *first* is
            // fixed rather than decided by a prefix hash. The failover case
            // needs some requests to land on the saturated engine first, and
            // affinity would send an identical prompt to the same one every
            // time -- which could be the idle one, proving nothing.
            "--policy",
            "round-robin",
        ])
        .env("FASTLLM_DATABASE_URL", database_url)
        .env("FASTLLM_PROXY_TOKEN", PROXY_TOKEN)
        .env("FASTLLM_ENCRYPTION_KEY", "ad".repeat(32))
        .env("FASTLLM_DATABASE_MAX_CONNECTIONS", "2")
        // The test's upstream engine is a loopback server; a deployment that
        // proxies to private backends says so exactly this way.
        .env("FASTLLM_SSRF_ACCEPT", "127.0.0.1,localhost")
        .spawn()
        .expect("failed to spawn fastllm-proxy --role all");
    let proc = Proc(child);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let up = |url: String| !matches!(ureq::get(&url).call(), Err(ureq::Error::Transport(_)));
        if up(format!("http://127.0.0.1:{port}/health"))
            && up(format!("http://127.0.0.1:{admin_port}/healthz"))
        {
            return proc;
        }
        assert!(
            Instant::now() < deadline,
            "fastllm-proxy did not come up within 20s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn admin(
    method: &str,
    admin_port: u16,
    cookie: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let req = ureq::request(method, &format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie);
    let resp = if method == "GET" {
        req.call()
    } else {
        req.send_json(body.clone())
    };
    match resp {
        Ok(r) => r.into_json().unwrap_or(serde_json::Value::Null),
        Err(ureq::Error::Status(code, r)) => panic!(
            "admin {method} {path} with {body} failed: {code} {}",
            r.into_string().unwrap_or_default()
        ),
        Err(e) => panic!("admin {method} {path} failed: {e}"),
    }
}

/// Everything a case needs: a model served by the given engines, a key that
/// may call it, and the ids of its backends in engine-port order.
struct Fixture {
    model: String,
    key: String,
    backend_ids: Vec<String>,
    cookie: String,
    admin_port: u16,
}

async fn provision(
    pool: &sqlx::PgPool,
    admin_port: u16,
    cookie: &str,
    suffix: &str,
    engine_ports: &[u16],
) -> Fixture {
    let model = format!("slow-{suffix}");
    let m = admin(
        "POST",
        admin_port,
        cookie,
        "/admin/provider-models",
        serde_json::json!({"name": model, "description": "admission e2e"}),
    );
    let model_id = m["id"].as_str().unwrap().to_string();
    for port in engine_ports {
        admin(
            "POST",
            admin_port,
            cookie,
            &format!("/admin/provider-models/{model_id}/backends"),
            serde_json::json!({"api_base": format!("http://127.0.0.1:{port}/v1")}),
        );
    }

    let models = admin(
        "GET",
        admin_port,
        cookie,
        "/admin/provider-models",
        serde_json::Value::Null,
    );
    let backends = models
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == model.as_str())
        .expect("model is listed")["backends"]
        .as_array()
        .unwrap()
        .clone();
    let backend_ids = engine_ports
        .iter()
        .map(|port| {
            backends
                .iter()
                .find(|b| b["api_base"] == format!("http://127.0.0.1:{port}/v1").as_str())
                .expect("backend is listed")["id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();

    let p = admin(
        "POST",
        admin_port,
        cookie,
        "/admin/principals",
        serde_json::json!({"name": format!("adm-principal-{suffix}")}),
    );
    let principal_id: uuid::Uuid = p["id"].as_str().unwrap().parse().unwrap();
    grant(pool, principal_id, &model, &format!("adm-role-{suffix}")).await;
    let k = admin(
        "POST",
        admin_port,
        cookie,
        "/admin/keys",
        serde_json::json!({"principal_id": principal_id, "name": format!("adm-key-{suffix}")}),
    );

    Fixture {
        model,
        key: k["key"].as_str().unwrap().to_string(),
        backend_ids,
        cookie: cookie.to_string(),
        admin_port,
    }
}

async fn grant(pool: &sqlx::PgPool, principal_id: uuid::Uuid, model: &str, role: &str) {
    let role_id: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO roles (name) VALUES ($1) RETURNING id")
            .bind(role)
            .fetch_one(pool)
            .await
            .unwrap();
    let permission_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO permissions (verb, resource) VALUES ('model:invoke', $1) RETURNING id",
    )
    .bind(format!("model/{model}"))
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO role_permissions (role_id, permission_id) VALUES ($1, $2)")
        .bind(role_id)
        .bind(permission_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO principal_roles (principal_id, role_id) VALUES ($1, $2)")
        .bind(principal_id)
        .bind(role_id)
        .execute(pool)
        .await
        .unwrap();
}

impl Fixture {
    /// What the Models page sends when an operator sets a gate.
    fn set_gate(&self, backend: usize, max_concurrent: u32) {
        admin(
            "PATCH",
            self.admin_port,
            &self.cookie,
            &format!("/admin/backends/{}", self.backend_ids[backend]),
            serde_json::json!({
                "admission_max_concurrent": max_concurrent,
                "admission_high_water": 4,
                "admission_max_queued": 1,
                "admission_max_wait_seconds": 1,
            }),
        );
    }

    fn metrics(&self, port: u16) -> String {
        ureq::get(&format!("http://127.0.0.1:{port}/metrics"))
            .set("authorization", &format!("Bearer {}", self.key))
            .call()
            .expect("scrape /metrics")
            .into_string()
            .unwrap()
    }

    /// Wait until the proxy's registry holds the gates this test set, and
    /// the scraper has acted on the engines' queues at least once.
    ///
    /// Polls `/metrics` rather than sleeping: the gate appearing there is the
    /// proof the setting travelled database -> snapshot -> running proxy,
    /// which is the path under test.
    fn wait_for_gates(&self, port: u16, gated_ports: &[u16], expect_capacity: &[u32]) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = self.metrics(port);
            let ready = gated_ports.iter().zip(expect_capacity).all(|(p, cap)| {
                text.lines().any(|l| {
                    l.starts_with("fastllm_admission_capacity{")
                        && l.contains(&format!("127.0.0.1:{p}/v1"))
                        && l.ends_with(&format!(" {cap}"))
                })
            });
            if ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "gates never reached the proxy with the expected ceilings; /metrics:\n{}",
                text.lines()
                    .filter(|l| l.contains("admission"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// `(status, retry-after, body)` for every request in a simultaneous burst.
fn burst(port: u16, key: &str, model: &str, n: usize) -> Vec<(u16, Option<String>, String)> {
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let (key, model) = (key.to_string(), model.to_string());
            std::thread::spawn(move || {
                let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
                    .set("authorization", &format!("Bearer {key}"))
                    .timeout(Duration::from_secs(20))
                    .send_json(serde_json::json!({
                        "model": model,
                        "messages": [{"role": "user", "content": "hi"}]
                    }));
                let r = match resp {
                    Ok(r) => r,
                    Err(ureq::Error::Status(_, r)) => r,
                    Err(e) => panic!("request failed outright: {e}"),
                };
                let status = r.status();
                let retry = r.header("retry-after").map(str::to_owned);
                let mut body = String::new();
                let _ = r.into_reader().read_to_string(&mut body);
                (status, retry, body)
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

/// Wait until the model is routable, so a burst does not race the snapshot.
fn wait_routable(port: u16, key: &str, model: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, ..) = burst(port, key, model, 1).remove(0);
        if status == 200 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{model} never became routable (last status {status})"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

async fn setup(
    port: u16,
    admin_port: u16,
    engines: &[(u16, Arc<Engine>)],
) -> (Proc, Fixture, TestCleanup) {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let suffix = suffix(port);
    let cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");
    for (p, e) in engines {
        spawn_engine(*p, Arc::clone(e)).await;
    }
    let admin_name = format!("adm-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let ports: Vec<u16> = engines.iter().map(|(p, _)| *p).collect();
    let fx = provision(&pool, admin_port, &cookie, &suffix, &ports).await;
    wait_routable(port, &fx.key, &fx.model);
    (proc, fx, cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_gate_set_in_the_admin_api_shields_a_drowning_engine() {
    let engine = Engine::reporting(50);
    let (port, admin_port) = (14841, 14842);
    let (_p, fx, _c) = setup(port, admin_port, &[(14843, Arc::clone(&engine))]).await;

    // Set while the proxy is already serving this backend. A gate that only
    // took effect on restart would fail here: the registry carries a live
    // backend forward unless its identity changed.
    fx.set_gate(0, 2);
    // Fifty waiting is far past high water, so 2 halves to 1.
    fx.wait_for_gates(port, &[14843], &[1]);
    engine.peak.store(0, Ordering::SeqCst);

    let (key, model) = (fx.key.clone(), fx.model.clone());
    let results = tokio::task::spawn_blocking(move || burst(port, &key, &model, 6))
        .await
        .unwrap();

    let peak = engine.peak.load(Ordering::SeqCst);
    assert!(
        peak <= 2,
        "the engine saw {peak} concurrent requests; the gate should hold it to 2 at most"
    );
    let refused: Vec<_> = results.iter().filter(|(s, ..)| *s == 503).collect();
    assert!(
        !refused.is_empty(),
        "six requests against a ceiling of one must refuse some: {results:?}"
    );
    for (_, retry, body) in &refused {
        assert!(
            retry.is_some(),
            "a refusal must carry Retry-After, or the caller retries at once: {body}"
        );
        assert!(
            body.contains("backend_overloaded"),
            "a refusal must say it is the backend, not the caller's quota: {body}"
        );
    }
    assert!(
        results.iter().any(|(s, ..)| *s == 200),
        "the gate must still admit something, or it is an outage: {results:?}"
    );

    // And an operator can see all of it.
    let text = fx.metrics(port);
    for series in [
        "fastllm_admission_max_concurrent{",
        "fastllm_admission_in_use{",
        "fastllm_admission_queued{",
        "fastllm_admission_admitted_total{",
        "fastllm_admission_refused_total{",
        "fastllm_backend_engine_requests{",
    ] {
        assert!(
            text.lines()
                .any(|l| l.starts_with(series) && l.contains("127.0.0.1:14843/v1")),
            "/metrics is missing {series} for this backend"
        );
    }
    // Scoped to this test's engine: the shared test database means /metrics
    // also lists other tests' backends.
    let ours = |l: &&str| l.contains("127.0.0.1:14843/v1");
    let refused_total: u64 = text
        .lines()
        .filter(ours)
        .filter(|l| l.starts_with("fastllm_admission_refused_total{"))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum();
    assert!(
        refused_total >= refused.len() as u64,
        "/metrics counted {refused_total} refusals; callers saw {}",
        refused.len()
    );
    assert!(
        text.lines()
            .filter(ours)
            .any(|l| l.contains("state=\"waiting\"} 50")),
        "the engine's own queue should be visible in /metrics"
    );

    // The dashboard reads the fleet report, not /metrics.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let fleet = admin(
            "GET",
            fx.admin_port,
            &fx.cookie,
            "/admin/fleet",
            serde_json::Value::Null,
        );
        let b = fleet
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["backends"].as_array().unwrap().clone())
            .find(|b| b["api_base"] == "http://127.0.0.1:14843/v1");
        if let Some(b) = b.filter(|b| b["admission"].is_object()) {
            assert_eq!(b["admission"]["max_concurrent"], 2, "{b}");
            assert_eq!(b["admission"]["capacity"], 1, "{b}");
            assert_eq!(b["engine_waiting"], 50, "{b}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "/admin/fleet never showed the gate: {fleet}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The control: same engine, same burst, no gate. The engine takes the lot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn without_a_gate_the_engine_takes_the_whole_burst() {
    let engine = Engine::reporting(50);
    let (port, admin_port) = (14851, 14852);
    let (_p, fx, _c) = setup(port, admin_port, &[(14853, Arc::clone(&engine))]).await;
    engine.peak.store(0, Ordering::SeqCst);

    let (key, model) = (fx.key.clone(), fx.model.clone());
    let results = tokio::task::spawn_blocking(move || burst(port, &key, &model, 6))
        .await
        .unwrap();

    assert!(
        results.iter().all(|(s, ..)| *s == 200),
        "with no gate nothing should be refused: {results:?}"
    );
    let peak = engine.peak.load(Ordering::SeqCst);
    assert!(
        peak >= 5,
        "with no gate the engine should see (nearly) the whole burst at once, saw {peak}"
    );
    // Only this test's engine: every `--role all` process here builds its
    // snapshot from the one shared test database, so /metrics also carries
    // the backends -- and gates -- that concurrently running tests set up.
    assert!(
        !fx.metrics(port)
            .lines()
            .any(|l| l.starts_with("fastllm_admission_") && l.contains("127.0.0.1:14853/v1")),
        "an ungated backend must not grow gate series"
    );
}

/// The gate must not defeat the pool it sits inside.
///
/// Two backends for one model: the first reports a deep queue, so its gate
/// shrinks to a single slot; the second is idle and keeps two. Three requests
/// is exactly the pool's capacity. Round-robin sends two of them to the
/// saturated engine first, and the one that finds it full has to move on to
/// the idle sibling -- not queue behind the saturated engine, and not be
/// refused while there is room next door.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_saturated_engine_hands_its_surplus_to_an_idle_sibling() {
    let drowning = Engine::reporting(50);
    let idle = Engine::reporting(0);
    let (port, admin_port) = (14861, 14862);
    let (_p, fx, _c) = setup(
        port,
        admin_port,
        &[(14863, Arc::clone(&drowning)), (14864, Arc::clone(&idle))],
    )
    .await;
    fx.set_gate(0, 2);
    fx.set_gate(1, 2);
    fx.wait_for_gates(port, &[14863, 14864], &[1, 2]);
    drowning.peak.store(0, Ordering::SeqCst);
    idle.peak.store(0, Ordering::SeqCst);

    let (key, model) = (fx.key.clone(), fx.model.clone());
    let results = tokio::task::spawn_blocking(move || burst(port, &key, &model, 3))
        .await
        .unwrap();

    assert!(
        results.iter().all(|(s, ..)| *s == 200),
        "the pool had room for all three, so none should be refused: {results:?}"
    );
    assert_eq!(
        drowning.peak.load(Ordering::SeqCst),
        1,
        "the saturated engine's gate shrank to one slot and must have held it there"
    );
    assert_eq!(
        idle.peak.load(Ordering::SeqCst),
        2,
        "the idle sibling should have taken its own request and the surplus"
    );
}
