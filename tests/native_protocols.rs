//! Native-protocol translation, end to end through the real binary.
//!
//! `src/protocol/tests.rs` asserts the translation itself on exact bytes. This
//! file asserts the part unit tests structurally cannot: that a backend
//! configured through the admin API as `protocol = anthropic` actually causes
//! the running proxy to speak the Messages API to it, present the credential
//! as `x-api-key` rather than a bearer token, and hand the client back
//! something an OpenAI client can read — including the streaming case, where
//! the response is re-framed on the fly.
//!
//! It also pins the boundary the whole design rests on: an `openai` backend's
//! response bytes reach the client **unchanged**. That is the property a
//! translation layer is most likely to erode, and it cannot be checked by
//! reading the translator.
//!
//! ```text
//! DATABASE_URL=$(cat /tmp/dburl) cargo test --features control -- --include-ignored
//! ```

use std::io::Read;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

const PROXY_TOKEN: &str = "native-protocols-e2e-proxy-token";
const UPSTREAM_KEY: &str = "sk-ant-test-credential";

fn encryption_key() -> String {
    "ee".repeat(32)
}

/// One suffix per test, appended to every name it mints, so a single
/// `track_suffix` per table cleans up everything the test created — see
/// `support::TestCleanup` for why leaving rows behind is not merely untidy.
///
/// Keyed on the test's own (unique) proxy port as well as the clock. A
/// timestamp alone is not unique: `as_nanos()` is coarse enough on macOS that
/// tests starting together in parallel read the *same* value, and three of
/// five then died on `principals_name_key`. The port disambiguates by
/// construction, the timestamp keeps successive runs from colliding with rows
/// an earlier run failed to clean up.
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
        .track_suffix("principals", "name", suffix)
        .track_suffix("api_keys", "name", suffix)
        // `grant_one_model` mints a role and a permission per test as well.
        .track_suffix("roles", "name", suffix)
        .track_suffix("permissions", "resource", suffix)
}

/// Grant `principal_id` `model:invoke` on exactly one model, through a role
/// created for the purpose.
///
/// The seeded `inference` role does not grant every model — that is the whole
/// point of the RBAC model — and no admin route grants a single model this
/// precisely, so this goes to Postgres directly, the same way
/// `tests/virtual_models.rs` does and for the same reason.
async fn grant_one_model(pool: &sqlx::PgPool, principal_id: i64, model: &str, role_name: &str) {
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

fn wait_healthy(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // Any HTTP answer proves the listener is up, including the 503
        // `/health` returns while no backend is healthy yet — treating only
        // 2xx as "started" is what made an earlier round of these tests hang
        // for the full timeout against an empty database.
        if !matches!(
            ureq::get(&format!("http://127.0.0.1:{port}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy on port {port} did not answer /health within 20s");
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
    wait_healthy(port);
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

/// What the mock upstream saw, so the test can assert on the request the
/// proxy actually sent rather than only on what came back.
#[derive(Default)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
    /// How many non-probe requests the mock has answered. The only way to
    /// assert a cache hit that does not depend on timing.
    hits: usize,
}

type Recorder = Arc<Mutex<Seen>>;

/// A mock provider that records the request it was given and replies with a
/// fixed, real-shaped native response.
async fn spawn_mock(port: u16, recorder: Recorder, response: MockResponse) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind mock upstream");
    let hits = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let recorder = Arc::clone(&recorder);
            let response = response.clone();
            let hits = Arc::clone(&hits);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let recorder = Arc::clone(&recorder);
                        let response = response.clone();
                        let hits = Arc::clone(&hits);
                        async move {
                            use http_body_util::BodyExt;
                            let path = req
                                .uri()
                                .path_and_query()
                                .map(|p| p.to_string())
                                .unwrap_or_default();
                            let headers: Vec<(String, String)> = req
                                .headers()
                                .iter()
                                .map(|(k, v)| {
                                    (
                                        k.as_str().to_string(),
                                        v.to_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect();
                            let is_probe = req.method() == hyper::Method::GET;
                            let body = req.into_body().collect().await.map(|b| b.to_bytes());
                            let body = body
                                .map(|b| String::from_utf8_lossy(&b).to_string())
                                .unwrap_or_default();
                            // Health probes are GETs on `/models` and must not
                            // clobber what the assertions are looking at.
                            let (content_type, payload) = if is_probe {
                                ("application/json", "{\"data\":[]}".to_string())
                            } else {
                                response.for_request(&body)
                            };
                            if !is_probe {
                                hits.fetch_add(1, Ordering::Relaxed);
                                let mut seen = recorder.lock().unwrap();
                                let hits = seen.hits + 1;
                                *seen = Seen {
                                    path,
                                    headers,
                                    body,
                                    hits,
                                };
                            }
                            Ok::<_, std::convert::Infallible>(
                                hyper::Response::builder()
                                    .status(200)
                                    .header("content-type", content_type)
                                    .body(http_body_util::Full::new(bytes::Bytes::from(payload)))
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

/// A provider answers a streaming request with SSE and a non-streaming one
/// with a JSON document. The mock has to do the same: a mock that always
/// replied with SSE made the harness's own non-streaming warm-up request fail
/// translation, which looked exactly like a product bug.
#[derive(Clone)]
struct MockResponse {
    json: String,
    sse: Option<String>,
}

impl MockResponse {
    fn json(body: &str) -> Self {
        Self {
            json: body.to_string(),
            sse: None,
        }
    }

    fn streaming(json: &str, sse: &str) -> Self {
        Self {
            json: json.to_string(),
            sse: Some(sse.to_string()),
        }
    }

    /// Chosen from what the upstream was actually asked for, not from what
    /// the test expects — so a proxy that failed to request streaming
    /// upstream would get a non-streaming answer and be caught.
    fn for_request(&self, body: &str) -> (&'static str, String) {
        match (&self.sse, body.contains("\"stream\":true")) {
            (Some(sse), true) => ("text/event-stream", sse.clone()),
            _ => ("application/json", self.json.clone()),
        }
    }
}

/// Create a model with one backend of the given protocol, and a key that may
/// invoke it. Returns `(key, model_name)`.
async fn provision(
    pool: &sqlx::PgPool,
    admin_port: u16,
    cookie: &str,
    suffix: &str,
    protocol: &str,
    api_base: &str,
    default_max_tokens: Option<u32>,
) -> (String, String) {
    let model = format!("np-{protocol}-{suffix}");
    let created = admin_post(
        admin_port,
        cookie,
        "/admin/models",
        serde_json::json!({"name": model, "description": "native protocol e2e"}),
    );
    let model_id = created["id"].as_i64().expect("model id");

    let mut backend = serde_json::json!({
        "api_base": api_base,
        "upstream_model": "provider-model-name",
        "upstream_api_key": UPSTREAM_KEY,
        "protocol": protocol,
    });
    if let Some(n) = default_max_tokens {
        backend["default_max_tokens"] = serde_json::json!(n);
    }
    admin_post(
        admin_port,
        cookie,
        &format!("/admin/models/{model_id}/backends"),
        backend,
    );

    let principal = format!("np-principal-{suffix}");
    let p = admin_post(
        admin_port,
        cookie,
        "/admin/principals",
        serde_json::json!({"name": principal, "roles": ["inference"]}),
    );
    let principal_id = p["id"].as_i64().expect("principal id");
    grant_one_model(pool, principal_id, &model, &format!("np-role-{suffix}")).await;
    let key_name = format!("np-key-{suffix}");
    let k = admin_post(
        admin_port,
        cookie,
        "/admin/keys",
        serde_json::json!({"principal_id": principal_id, "name": key_name}),
    );
    (k["key"].as_str().expect("key").to_string(), model)
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

/// Wait for a newly created backend to appear in the proxy's snapshot.
///
/// The rebuild runs on its own 1s schedule, so a request sent immediately
/// after provisioning races it and fails with "model not served here" — a
/// harness bug that reads exactly like a product one.
fn wait_for_model(port: u16, key: &str, model: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, body) = chat(
            port,
            key,
            serde_json::json!({
                "model": model,
                "max_tokens": 4,
                "messages": [{"role": "user", "content": "ping"}],
            }),
        );
        if status != 404 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("model {model} never reached the snapshot: {status} {body}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

const ANTHROPIC_BODY: &str = r#"{"id":"msg_e2e","type":"message","role":"assistant","content":[{"type":"text","text":"translated hello"}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":5}}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn an_anthropic_backend_receives_the_messages_api_and_answers_an_openai_client() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14711, 14712, 14713);

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::json(ANTHROPIC_BODY),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    let (key, model) = provision(
        &pool,
        admin_port,
        &cookie,
        &suffix,
        "anthropic",
        &format!("http://127.0.0.1:{upstream_port}/v1"),
        Some(256),
    )
    .await;
    wait_for_model(port, &key, &model);

    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "max_tokens": 64,
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "Say hello"},
            ],
        }),
    );
    assert_eq!(status, 200, "{body}");

    // What the provider was sent.
    let seen = recorder.lock().unwrap();
    assert_eq!(seen.path, "/v1/messages", "Messages API path");
    let sent: serde_json::Value = serde_json::from_str(&seen.body).expect("upstream body is JSON");
    assert_eq!(sent["model"], serde_json::json!("provider-model-name"));
    // A content block rather than a bare string, so it can carry the cache
    // breakpoint that makes a repeated system prompt cost 90% less.
    assert_eq!(
        sent["system"],
        serde_json::json!([{
            "type": "text",
            "text": "Be terse.",
            "cache_control": {"type": "ephemeral"},
        }])
    );
    assert_eq!(sent["max_tokens"], serde_json::json!(64));
    assert_eq!(
        sent["messages"],
        serde_json::json!([{"role": "user", "content": "Say hello"}])
    );

    // How it was authenticated. The bearer token the *client* used must not
    // leak upstream, and the provider's own key must arrive raw in x-api-key.
    let header = |name: &str| {
        seen.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("x-api-key").as_deref(), Some(UPSTREAM_KEY));
    assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
    assert_ne!(
        header("authorization").as_deref(),
        Some(format!("Bearer {key}").as_str()),
        "the client's key must never be forwarded to the provider"
    );
    drop(seen);

    // What the client got back.
    let got: serde_json::Value = serde_json::from_str(&body).expect("client body is JSON");
    assert_eq!(got["object"], serde_json::json!("chat.completion"));
    assert_eq!(got["model"], serde_json::json!(model));
    assert_eq!(
        got["choices"][0]["message"]["content"],
        serde_json::json!("translated hello")
    );
    assert_eq!(
        got["choices"][0]["finish_reason"],
        serde_json::json!("stop")
    );
    assert_eq!(got["usage"]["prompt_tokens"], serde_json::json!(12));
    assert_eq!(got["usage"]["completion_tokens"], serde_json::json!(5));
}

const ANTHROPIC_SSE: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_stream\",\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
    "event: ping\ndata: {\"type\":\"ping\"}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"str\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"eamed\"}}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn an_anthropic_stream_is_reframed_into_openai_chunks_for_the_client() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14721, 14722, 14723);

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::streaming(ANTHROPIC_BODY, ANTHROPIC_SSE),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    let (key, model) = provision(
        &pool,
        admin_port,
        &cookie,
        &suffix,
        "anthropic",
        &format!("http://127.0.0.1:{upstream_port}/v1"),
        Some(256),
    )
    .await;
    wait_for_model(port, &key, &model);

    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "max_tokens": 32,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "stream please"}],
        }),
    );
    assert_eq!(status, 200, "{body}");

    assert!(
        recorder.lock().unwrap().body.contains("\"stream\":true"),
        "streaming must be requested upstream, not faked locally"
    );

    // Reassemble as an OpenAI client would.
    let mut content = String::new();
    let mut finish = None;
    let mut usage = None;
    let mut saw_done = false;
    for frame in body.split("\n\n") {
        let Some(data) = frame.trim().strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            saw_done = true;
            continue;
        }
        let chunk: serde_json::Value = serde_json::from_str(data).expect("each frame is JSON");
        assert_eq!(chunk["object"], serde_json::json!("chat.completion.chunk"));
        if let Some(c) = chunk["choices"][0]["delta"]["content"].as_str() {
            content.push_str(c);
        }
        if let Some(r) = chunk["choices"][0]["finish_reason"].as_str() {
            finish = Some(r.to_string());
        }
        if chunk["usage"].is_object() {
            usage = Some(chunk["usage"].clone());
        }
    }
    assert_eq!(content, "streamed");
    assert_eq!(finish.as_deref(), Some("stop"));
    assert!(saw_done, "a client without [DONE] waits forever");
    assert_eq!(
        usage,
        Some(serde_json::json!({
            "prompt_tokens": 9, "completion_tokens": 6, "total_tokens": 15
        })),
        "usage comes from the provider's own numbers, parsed exactly"
    );
}

const GEMINI_BODY: &str = r#"{"candidates":[{"content":{"parts":[{"text":"gemini says hi"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":4},"responseId":"resp_e2e"}"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_gemini_backend_is_addressed_by_url_and_authenticated_with_x_goog_api_key() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14731, 14732, 14733);

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::json(GEMINI_BODY),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    let (key, model) = provision(
        &pool,
        admin_port,
        &cookie,
        &suffix,
        "gemini",
        &format!("http://127.0.0.1:{upstream_port}/v1beta"),
        None,
    )
    .await;
    wait_for_model(port, &key, &model);

    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }),
    );
    assert_eq!(status, 200, "{body}");

    let seen = recorder.lock().unwrap();
    // The model lives in the URL for Gemini, which is the whole reason
    // request translation decides the path as well as the payload.
    assert_eq!(
        seen.path,
        "/v1beta/models/provider-model-name:generateContent"
    );
    assert_eq!(
        seen.headers
            .iter()
            .find(|(k, _)| k == "x-goog-api-key")
            .map(|(_, v)| v.as_str()),
        Some(UPSTREAM_KEY),
        "Gemini reads a raw key from x-goog-api-key, not a bearer token"
    );
    let sent: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
    assert_eq!(
        sent["contents"],
        serde_json::json!([{"role": "user", "parts": [{"text": "hi"}]}])
    );
    drop(seen);

    let got: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        got["choices"][0]["message"]["content"],
        serde_json::json!("gemini says hi")
    );
    assert_eq!(got["usage"]["total_tokens"], serde_json::json!(11));
}

/// Refusals have to survive the whole stack, not just the translator: an
/// operator who sees a 200 with tools silently dropped has no way to know.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn unsupported_requests_are_refused_with_501_naming_the_feature() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14741, 14742, 14743);

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::json(ANTHROPIC_BODY),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    // No default_max_tokens: the omitted-max_tokens case must be refused, not
    // silently capped.
    let (key, model) = provision(
        &pool,
        admin_port,
        &cookie,
        &suffix,
        "anthropic",
        &format!("http://127.0.0.1:{upstream_port}/v1"),
        None,
    )
    .await;
    wait_for_model(port, &key, &model);

    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            // The pre-2023 spelling. Refused rather than given a second
            // mapping of its own — every client that still emits it also
            // accepts `tools`, which is translated.
            "functions": [{"name": "f"}],
        }),
    );
    assert_eq!(status, 501, "{body}");
    assert!(
        body.contains("`functions`"),
        "the refusal must name what was unsupported: {body}"
    );

    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }),
    );
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("default_max_tokens"),
        "the refusal must say how to fix it: {body}"
    );

    // Endpoints with no native equivalent fail locally and clearly rather
    // than being forwarded to a provider that cannot read them.
    let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/embeddings"))
        .set("authorization", &format!("Bearer {key}"))
        .send_json(serde_json::json!({"model": model, "input": "x"}));
    let status = match resp {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, _)) => code,
        Err(e) => panic!("embeddings request failed: {e}"),
    };
    assert_eq!(status, 501, "/embeddings has no Anthropic equivalent");
}

/// The invariant the rest of the proxy is built on.
///
/// An `openai` backend's response must reach the client byte for byte. This
/// deliberately uses a payload nothing would produce by accident — odd
/// whitespace, key order that no serializer of ours would emit, a field the
/// proxy has no struct for — so that any round trip through a parse-and-
/// re-serialise path shows up as a difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_passthrough_backend_is_forwarded_byte_for_byte() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14751, 14752, 14753);

    const ODD_BUT_VALID: &str = "{\"zzz_unknown_field\":[1,2,3],   \"object\":\"chat.completion\",\n\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"verbatim\"}}],\"id\":\"cmpl-passthrough\",\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}";

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::json(ODD_BUT_VALID),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    let (key, model) = provision(
        &pool,
        admin_port,
        &cookie,
        &suffix,
        "openai",
        &format!("http://127.0.0.1:{upstream_port}/v1"),
        None,
    )
    .await;
    wait_for_model(port, &key, &model);

    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body, ODD_BUT_VALID,
        "an openai backend's response must not be parsed, reordered or re-serialised"
    );

    // And the request goes out untouched apart from the model rewrite the
    // proxy has always done for aliases.
    let seen = recorder.lock().unwrap();
    assert_eq!(seen.path, "/v1/chat/completions");
    assert_eq!(
        seen.headers
            .iter()
            .find(|(k, _)| k == "authorization")
            .map(|(_, v)| v.as_str()),
        Some(format!("Bearer {UPSTREAM_KEY}").as_str()),
        "an openai backend still gets a bearer token"
    );
}

const ANTHROPIC_TOOL_BODY: &str = r#"{"id":"msg_tool","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_e2e","name":"get_weather","input":{"city":"Paris"}}],"stop_reason":"tool_use","usage":{"input_tokens":20,"output_tokens":9}}"#;

/// Tool calling end to end, through a real proxy rather than the translator
/// alone: the client speaks OpenAI, the provider speaks Anthropic, and the
/// second turn carries a transcript that only a real message mapper can send —
/// a call on the assistant message and its result in a separate one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn a_tool_call_round_trips_through_an_anthropic_backend() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14761, 14762, 14763);

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::json(ANTHROPIC_TOOL_BODY),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);
    let (key, model) = provision(
        &pool,
        admin_port,
        &cookie,
        &suffix,
        "anthropic",
        &format!("http://127.0.0.1:{upstream_port}/v1"),
        Some(64),
    )
    .await;
    wait_for_model(port, &key, &model);

    // Turn one: offer a tool, get a call back.
    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "Weather in Paris?"}],
            "tools": [{"type": "function", "function": {
                "name": "get_weather",
                "description": "Current weather",
                "parameters": {"type": "object",
                    "properties": {"city": {"type": "string"}}, "required": ["city"]},
            }}],
        }),
    );
    assert_eq!(status, 200, "{body}");

    let sent: serde_json::Value =
        serde_json::from_str(&recorder.lock().unwrap().body).expect("upstream body is JSON");
    assert_eq!(
        sent["tools"],
        serde_json::json!([{
            "name": "get_weather",
            "description": "Current weather",
            "input_schema": {"type": "object",
                "properties": {"city": {"type": "string"}}, "required": ["city"]},
        }]),
        "the provider must receive `input_schema`, not `parameters`"
    );

    let out: serde_json::Value = serde_json::from_str(&body).expect("response is JSON");
    assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        out["choices"][0]["message"]["content"],
        serde_json::Value::Null
    );
    let call = &out["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], "toolu_e2e");
    assert_eq!(call["function"]["name"], "get_weather");
    assert_eq!(
        call["function"]["arguments"], "{\"city\":\"Paris\"}",
        "arguments reach an OpenAI client as a string"
    );
    assert_eq!(out["usage"]["prompt_tokens"], 20);
    assert_eq!(out["usage"]["completion_tokens"], 9);

    // Turn two: hand the call and its result back, the way a client does.
    let (status, body) = chat(
        port,
        &key,
        serde_json::json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "Weather in Paris?"},
                {"role": "assistant", "content": null, "tool_calls": [call]},
                {"role": "tool", "tool_call_id": "toolu_e2e", "content": "{\"c\":17}"},
            ],
        }),
    );
    assert_eq!(status, 200, "{body}");

    let sent: serde_json::Value =
        serde_json::from_str(&recorder.lock().unwrap().body).expect("upstream body is JSON");
    assert_eq!(
        sent["messages"],
        serde_json::json!([
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_e2e", "name": "get_weather",
                 "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_e2e", "content": "{\"c\":17}"}
            ]},
        ]),
        "the result must be nested into a message and paired back by id"
    );
}

fn hits_of(recorder: &Recorder) -> usize {
    recorder.lock().unwrap().hits
}

/// A cached answer must never reach the provider a second time.
///
/// Asserted against the mock's own hit counter rather than against latency:
/// "it was faster" is a measurement that passes for the wrong reasons on a
/// loaded machine, where "the upstream was called once" cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires postgres"]
async fn an_identical_request_is_answered_from_cache_without_touching_the_provider() {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (port, admin_port, upstream_port) = (14791, 14792, 14793);

    let suffix = suffix(port);
    let _cleanup = cleanup_for(&suffix);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect to postgres");

    let recorder: Recorder = Arc::new(Mutex::new(Seen::default()));
    spawn_mock(
        upstream_port,
        Arc::clone(&recorder),
        MockResponse::json(r#"{"id":"a","object":"chat.completion","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#),
    )
    .await;

    let admin_name = format!("np-admin-{suffix}");
    support::bootstrap_login_user(&pool, &admin_name).await;
    let _proc = start_all(port, admin_port, &database_url);
    let cookie = support::login_cookie(admin_port, &admin_name);

    // An `openai` backend, so the response is the byte-exact passthrough path.
    let model = format!("cache-model-{suffix}");
    let created = admin_post(
        admin_port,
        &cookie,
        "/admin/models",
        serde_json::json!({"name": model, "description": "", "cache_ttl_seconds": 60}),
    );
    let model_id = created["id"].as_i64().expect("model id");
    // Localises a failure: a cache miss can mean the TTL never reached the
    // database, never reached the snapshot, or never reached the lookup.
    let stored: Option<i32> =
        sqlx::query_scalar("SELECT cache_ttl_seconds FROM models WHERE id = $1")
            .bind(model_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(60), "the admin API must persist the TTL");
    admin_post(
        admin_port,
        &cookie,
        &format!("/admin/models/{model_id}/backends"),
        serde_json::json!({
            "api_base": format!("http://127.0.0.1:{upstream_port}/v1"),
            "upstream_model": "m",
            "upstream_api_key": UPSTREAM_KEY,
        }),
    );

    let principal = format!("cache-sa-{suffix}");
    let principal_id: i64 = sqlx::query_scalar(
        "INSERT INTO principals (kind, name) VALUES ('service_account', $1) RETURNING id",
    )
    .bind(&principal)
    .fetch_one(&pool)
    .await
    .unwrap();
    // Suffixed, so `cleanup_for` reclaims it: an unsuffixed role leaks into
    // the shared database and breaks the migration test's exact role list.
    grant_one_model(&pool, principal_id, &model, &format!("cache-role-{suffix}")).await;
    let key = admin_post(
        admin_port,
        &cookie,
        "/admin/keys",
        serde_json::json!({"principal_id": principal_id, "name": "cache"}),
    )["key"]
        .as_str()
        .expect("key")
        .to_string();
    wait_for_model(port, &key, &model);

    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "the same question twice"}],
    });

    let before = hits_of(&recorder);
    let (status, first) = chat(port, &key, body.clone());
    assert_eq!(status, 200, "{first}");
    let after_first = hits_of(&recorder);
    assert_eq!(
        after_first,
        before + 1,
        "the first request reaches upstream"
    );

    let (status, second) = chat(port, &key, body);
    assert_eq!(status, 200, "{second}");
    assert_eq!(
        hits_of(&recorder),
        after_first,
        "the second must be served from cache, not forwarded"
    );
    assert_eq!(first, second, "and byte-for-byte the same answer");
}
