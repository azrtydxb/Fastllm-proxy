//! P3: usage accounting and budgets.
//!
//! Two tiers, same split the rest of this test suite uses:
//!
//! - Fast, no database: `File` mode's `budget:` config (`src/config.rs`) is
//!   the local mirror of the control plane's `budgets` table, resolved into
//!   exactly the `Principal::budget` the request path enforces
//!   (`src/source/file.rs`). These exercise enforcement itself —
//!   over-budget refuses, under-budget passes — without needing Postgres or
//!   a real upstream.
//! - `--role all` against real Postgres and a real (mock) upstream, in the
//!   style of `tests/frontend_models.rs`: a principal completes one request,
//!   the mock upstream's real `usage` object is read back through the tail
//!   buffer and reported, the control plane folds it into `budgets`, and the
//!   *next* request is refused — proving the whole after-the-fact loop the
//!   design doc describes, not just the enforcement half.
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) cargo test --features control -- --include-ignored
//! ```

use std::io::Read;
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

fn start_file_mode(config: &str, port: u16) -> Proc {
    let path = std::env::temp_dir().join(format!("budgets-{port}.yaml"));
    std::fs::write(&path, config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
        .args([
            "--config",
            path.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--role",
            "proxy",
        ])
        .spawn()
        .expect("failed to spawn fastllm-proxy");
    let proc = Proc(child);
    wait_healthy(port);
    proc
}

fn wait_healthy(port: u16) {
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
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy on port {port} did not answer /health within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn post(port: u16, key: &str, model: &str) -> u16 {
    let req = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"));
    match req.send_json(ureq::json!({ "model": model, "messages": [] })) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, resp)) => {
            let mut buf = String::new();
            let _ = resp.into_reader().read_to_string(&mut buf);
            code
        }
        Err(e) => panic!("request failed: {e}"),
    }
}

const FILE_MODE_CONFIG: &str = "\
model_list:
  - model_name: m
    litellm_params: { api_base: http://127.0.0.1:8198/v1 }
auth:
  keys:
    - key: sk-under-budget
      name: under-budget
      models: ['*']
      budget: { tokens_total: 1000, tokens_used: 0 }
    - key: sk-over-budget
      name: over-budget
      models: ['*']
      budget: { tokens_total: 10, tokens_used: 10 }
";

/// A principal whose consumption is already at (or past) its budget is
/// refused before the request ever reaches an upstream — the enforcement
/// half of P3, independent of how `tokens_used` got there.
#[test]
fn an_over_budget_key_is_refused_with_402() {
    let _p = start_file_mode(FILE_MODE_CONFIG, 14611);
    assert_eq!(post(14611, "sk-over-budget", "m"), 402);
}

/// A principal comfortably under its budget is admitted — the request still
/// fails downstream (nothing is listening on :8198 in this test), but with
/// something other than 402, proving the budget check itself let it through.
#[test]
fn an_under_budget_key_is_admitted() {
    let _p = start_file_mode(FILE_MODE_CONFIG, 14612);
    assert_ne!(post(14612, "sk-under-budget", "m"), 402);
}

// --- End-to-end against a real database and a real mock upstream --------

const PROXY_TOKEN: &str = "budgets-e2e-proxy-token";

fn encryption_key() -> String {
    "ee".repeat(32)
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
            // Fast enough that the test does not have to wait long for a
            // usage report (flushed on its own 5s schedule, see
            // `usage::spawn`'s caller in `main.rs::run_all`) to reach the
            // published snapshot as an updated `Principal.budget`.
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
    wait_healthy(port);
    proc
}

// `/admin/*` is session-gated (P4), not proxy-token-gated — see
// `support::login_cookie`. `PROXY_TOKEN` below is still what `/usage`
// requires, since that route (a proxy process reporting to the control
// plane) is deliberately unchanged.
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

fn admin_put(admin_port: u16, cookie: &str, path: &str, body: serde_json::Value) {
    let resp = ureq::put(&format!("http://127.0.0.1:{admin_port}{path}"))
        .set("cookie", cookie)
        .send_json(body.clone());
    if let Err(e) = resp {
        panic!("admin PUT {path} with {body} failed: {e}");
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

fn unique_name(tag: &str) -> String {
    format!(
        "{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// A minimal, real vLLM-shaped upstream that answers every request with a
/// non-streaming completion carrying a fixed, real `usage` object — the
/// thing the tail buffer (`crate::tail_buffer`) has to find without this
/// test's proxy under test ever parsing a frame in flight.
async fn spawn_mock_upstream(port: u16, prompt_tokens: u64, completion_tokens: u64) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock upstream");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| async move {
                        use http_body_util::BodyExt;
                        let _ = req.into_body().collect().await;
                        let body = serde_json::json!({
                            "id": "cmpl-mock",
                            "object": "chat.completion",
                            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}}],
                            "usage": {
                                "prompt_tokens": prompt_tokens,
                                "completion_tokens": completion_tokens,
                                "total_tokens": prompt_tokens + completion_tokens,
                            }
                        });
                        let bytes = bytes::Bytes::from(body.to_string());
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(200)
                                .header("content-type", "application/json")
                                .body(http_body_util::Full::new(bytes))
                                .unwrap(),
                        )
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

/// The end-to-end version of the design doc's whole P3 story: a completed
/// request's *real*, upstream-reported token count — not the request path's
/// coarse estimate — is what pushes a principal over budget, and it is the
/// *next* request that pays for it, never the one that went over.
// Multi-threaded on purpose. The mock upstream's accept loop lives on this
// runtime, and the assertions below drive the proxy with *blocking* ureq
// calls. On the default current-thread runtime those blocking calls starve
// the accept loop, so the upstream never answers, the proxy times out
// waiting for response headers, and the request fails as a 502 that looks
// like a product bug rather than a test-harness one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires postgres"]
async fn a_principal_with_a_tiny_budget_is_refused_after_exceeding_it() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let port = 14621;
    let admin_port = 14622;
    let upstream_port = 14623;

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .unwrap();
    let admin_user = unique_name("budget-e2e-admin");
    support::bootstrap_login_user(&pool, &admin_user).await;

    // Deletes the model, principal and login user this test creates (and,
    // by cascade, the backend/budget/key/usage/session rows hanging off
    // them) even if one of the `panic!`s below fires — this suite runs
    // against the shared kw dev database, which has no per-run reset, so a
    // cleanup line placed after the assertions instead of in a `Drop` guard
    // would leave rows behind on every failing run. Tracked by the same
    // literal tag `unique_name` below prefixes each name with, since
    // (unlike `frontend_models.rs`) each call mints its own timestamp rather
    // than sharing one suffix.
    let _cleanup = TestCleanup::new()
        .track_prefix("provider_models", "name", "budget-e2e-model")
        .track_prefix("principals", "name", "budget-e2e-principal")
        .track_prefix("principals", "name", "budget-e2e-admin");

    // 3 + 4 = 7 tokens per completed request.
    spawn_mock_upstream(upstream_port, 3, 4).await;
    let _p = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_user);

    let model_name = unique_name("budget-e2e-model");
    let model = admin_post(
        admin_port,
        &cookie,
        "/admin/provider-models",
        serde_json::json!({ "name": model_name }),
    );
    let provider_model_id = model["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/provider-models/{provider_model_id}/backends"),
        serde_json::json!({ "api_base": format!("http://127.0.0.1:{upstream_port}/v1") }),
    );

    let principal_name = unique_name("budget-e2e-principal");
    let principal = admin_post(
        admin_port,
        &cookie,
        "/admin/principals",
        serde_json::json!({ "name": principal_name }),
    );
    let principal_id = principal["id"].as_i64().unwrap();
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/principals/{principal_id}/roles"),
        serde_json::json!({ "role": "inference" }),
    );

    // A budget smaller than one completed request's real usage (7 tokens):
    // the first request must still be admitted (0 used so far) and must
    // still complete -- P3's stated tradeoff is that enforcement is
    // after-the-fact, so a request that blows the budget is not cut short.
    admin_put(
        admin_port,
        &cookie,
        &format!("/admin/principals/{principal_id}/budget"),
        serde_json::json!({ "tokens_total": 5, "window": "daily" }),
    );

    let key_resp = admin_post(
        admin_port,
        &cookie,
        "/admin/keys",
        serde_json::json!({ "name": "budget-e2e-key", "principal_id": principal_id }),
    );
    let key = key_resp["key"].as_str().unwrap().to_string();

    let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"))
        .send_json(ureq::json!({ "model": model_name, "messages": [] }));
    assert_eq!(
        resp.expect("the first request must complete").status(),
        200,
        "0 tokens used so far must be well under a budget of 5"
    );

    // Usage flows: proxy tail-buffers the response -> `usage::record` ->
    // flushed to `POST /usage` on its own schedule -> folded into
    // `budgets.tokens_used` -> picked up by the next snapshot rebuild. Poll
    // rather than a fixed sleep, since none of those schedules are
    // synchronised with this test.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let budgets = admin_get(admin_port, &cookie, "/admin/budgets");
        let mine = budgets
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["principal_id"].as_i64() == Some(principal_id));
        if let Some(b) = mine {
            if b["tokens_used"].as_i64().unwrap_or(0) >= 5 {
                break;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "budget for principal {principal_id} was never observed to exceed its total \
                 within 20s; usage reporting did not reach the published snapshot"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Poll for the refusal rather than asserting on a single shot. The
    // database now says the budget is spent, but the request path reads the
    // *snapshot*, which the control plane rebuilds on its own schedule — so
    // there is a bounded window where the row is over budget and the
    // published snapshot does not know it yet. That lag is the design's
    // stated eventual consistency, not a defect, and a test that asserts
    // instant propagation is asserting something the system never promised.
    // What it must prove is that enforcement *arrives*.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .set("authorization", &format!("Bearer {key}"))
            .send_json(ureq::json!({ "model": model_name, "messages": [] }));
        match resp {
            Err(ureq::Error::Status(402, _)) => break,
            other => {
                if Instant::now() >= deadline {
                    panic!(
                        "expected 402 once the budget is exhausted and the snapshot caught up; \
                         still getting {other:?} after 15s"
                    );
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}
