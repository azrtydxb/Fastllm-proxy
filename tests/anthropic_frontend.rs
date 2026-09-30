//! Three user-visible behaviours, end to end through the real binary:
//!
//! * #30 -- an Anthropic client (`x-api-key`, `POST /v1/messages`) is served,
//!   streaming and not, and gets Anthropic-shaped errors.
//! * #28 -- an upstream that answers `200` with an error document is reported
//!   with the status the document declares, so clients retry.
//! * #29 -- a backend whose health probe fails but which serves requests is
//!   still used when it is all a model has, instead of answering 502.
//!
//! One mock upstream stands in for all of them and decides what to do from the
//! model named in the request, which the proxy passes through unchanged for a
//! file-configured backend.

use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const MOCK_PORT: u16 = 14741;
const PROXY_PORT: u16 = 14740;

async fn spawn_mock(port: u16) {
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
                    |req: hyper::Request<hyper::body::Incoming>| async move {
                        use http_body_util::BodyExt;
                        let path = req.uri().path().to_string();
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        let text = String::from_utf8_lossy(&body).to_string();
                        let (status, ctype, out) = if path.ends_with("/models") {
                            // Fails for the model list a prober reads, while
                            // the completions below work: #29.
                            (500, "application/json", r#"{"error":"probe"}"#.to_string())
                        } else if text.contains("\"model\":\"masked\"") {
                            (
                                200,
                                "application/json",
                                r#"{"error":{"message":"Upstream error from Nvidia: ResourceExhausted","code":502}}"#
                                    .to_string(),
                            )
                        } else if text.contains("\"stream\":true") {
                            (
                                200,
                                "text/event-stream",
                                concat!(
                                    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
                                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n",
                                    "data: [DONE]\n\n",
                                )
                                .to_string(),
                            )
                        } else {
                            (
                                200,
                                "application/json",
                                r#"{"id":"chatcmpl-2","choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"Hello"}}],"usage":{"prompt_tokens":4,"completion_tokens":2}}"#
                                    .to_string(),
                            )
                        };
                        Ok::<_, std::convert::Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .header("content-type", ctype)
                                .body(http_body_util::Full::new(bytes::Bytes::from(out)))
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

fn start_proxy() -> Proc {
    let config = format!(
        "model_list:\n  - model_name: chat\n    litellm_params: {{ api_base: http://127.0.0.1:{MOCK_PORT}/v1 }}\n  - model_name: masked\n    litellm_params: {{ api_base: http://127.0.0.1:{MOCK_PORT}/v1 }}\n"
    );
    let path = std::env::temp_dir().join(format!("anthropic-frontend-{PROXY_PORT}.yaml"));
    std::fs::write(&path, config).unwrap();
    let proc = Proc(
        Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
            .args([
                "--config",
                path.to_str().unwrap(),
                "--port",
                &PROXY_PORT.to_string(),
                "--role",
                "proxy",
                "--health-interval",
                "1",
                "--health-timeout",
                "1",
                "--engine-scrape-interval",
                "0",
            ])
            .spawn()
            .expect("failed to spawn fastllm-proxy"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if ureq::get(&format!("http://127.0.0.1:{PROXY_PORT}/livez"))
            .call()
            .is_ok()
        {
            return proc;
        }
        assert!(Instant::now() < deadline, "proxy did not come up in 20s");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `(status, body)` for a POST, whatever the status.
fn post(path: &str, body: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut req = ureq::post(&format!("http://127.0.0.1:{PROXY_PORT}{path}"))
        .set("content-type", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    match req.send_string(body) {
        Ok(r) => (r.status(), r.into_string().unwrap()),
        Err(ureq::Error::Status(code, r)) => (code, r.into_string().unwrap()),
        Err(e) => panic!("POST {path} failed: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn anthropic_clients_are_served_and_upstream_errors_carry_their_status() {
    spawn_mock(MOCK_PORT).await;
    let _proxy = start_proxy();

    tokio::task::spawn_blocking(|| {
        let key = [("x-api-key", "k"), ("anthropic-version", "2023-06-01")];

        // #30, non-streaming.
        let (status, body) = post(
            "/v1/messages",
            r#"{"model":"chat","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
            &key,
        );
        assert_eq!(status, 200, "{body}");
        let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["model"], "chat");
        assert_eq!(msg["content"][0]["text"], "Hello");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["usage"]["input_tokens"], 4);

        // #30, streaming: the event sequence, in order.
        let (status, body) = post(
            "/v1/messages",
            r#"{"model":"chat","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
            &key,
        );
        assert_eq!(status, 200, "{body}");
        let events: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("event: "))
            .collect();
        assert_eq!(
            events,
            [
                "message_start",
                "ping",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ],
            "{body}"
        );

        // #30, count_tokens.
        let (status, body) = post(
            "/v1/messages/count_tokens",
            r#"{"model":"chat","messages":[{"role":"user","content":"hello there"}]}"#,
            &key,
        );
        assert_eq!(status, 200, "{body}");
        assert!(serde_json::from_str::<serde_json::Value>(&body).unwrap()["input_tokens"]
            .as_u64()
            .is_some_and(|n| n > 0));

        // #28: the OpenAI path reports the status the disguised error declares.
        let (status, body) = post(
            "/v1/chat/completions",
            r#"{"model":"masked","messages":[{"role":"user","content":"hi"}]}"#,
            &[],
        );
        assert_eq!(status, 502, "a 200 carrying an error document: {body}");
        assert!(body.contains("ResourceExhausted"), "{body}");

        // ...and the Anthropic path reports it in Anthropic's shape.
        let (status, body) = post(
            "/v1/messages",
            r#"{"model":"masked","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
            &key,
        );
        assert_eq!(status, 502, "{body}");
        let err: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(err["type"], "error");
        assert_eq!(err["error"]["type"], "api_error");

        // #29: the mock fails every probe, so after a few sweeps the health
        // prober has ejected the only backend `chat` has. It still serves.
        std::thread::sleep(Duration::from_secs(5));
        let (status, body) = post(
            "/v1/chat/completions",
            r#"{"model":"chat","messages":[{"role":"user","content":"hi"}]}"#,
            &[],
        );
        assert_eq!(
            status, 200,
            "an ejected backend should be the last resort, not a 502: {body}"
        );
    })
    .await
    .unwrap();
}
