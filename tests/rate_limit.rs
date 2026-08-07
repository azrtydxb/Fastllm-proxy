//! End-to-end rate limiting against a real spawned binary, in the style of
//! `tests/rbac.rs`.
//!
//! `File` mode is what makes this tractable without a database (see that
//! test's doc comment for the same reasoning): a `KeyConfig::limits` entry
//! (`src/config.rs`) is `File` mode's mirror of the control plane's `limits`
//! table, resolved into exactly the same `Principal::limits` the request
//! path reads (`src/source/file.rs`).

use std::io::Read;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(config: &str, port: u16) -> Proc {
    let path = std::env::temp_dir().join(format!("rate-limit-{port}.yaml"));
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
            return proc;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy on port {port} did not answer /health within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

const CONFIG: &str = "\
model_list:
  - model_name: m
    litellm_params: { api_base: http://127.0.0.1:8199/v1 }
auth:
  keys:
    - key: sk-limited
      name: limited
      models: ['*']
      limits: { requests_per_min: 3 }
    - key: sk-unlimited
      name: unlimited
      models: ['*']
";

fn post(port: u16, key: &str) -> (u16, Option<String>) {
    let req = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .set("authorization", &format!("Bearer {key}"));
    match req.send_json(ureq::json!({ "model": "m", "messages": [] })) {
        Ok(r) => (r.status(), r.header("retry-after").map(str::to_string)),
        Err(ureq::Error::Status(code, resp)) => {
            let retry_after = resp.header("retry-after").map(str::to_string);
            // Drain the body so the connection can be reused by the next
            // request on this same short-lived agent.
            let mut buf = String::new();
            let _ = resp.into_reader().read_to_string(&mut buf);
            (code, retry_after)
        }
        Err(e) => panic!("request failed: {e}"),
    }
}

/// The end-to-end version of `crate::limiter`'s unit tests: a key configured
/// with a small `requests_per_min` limit gets exactly its allowance and then
/// 429s, with a `Retry-After` header a client can act on.
#[test]
fn a_key_with_a_small_limit_gets_429_after_its_allowance() {
    let _p = start(CONFIG, 14511);

    for i in 0..3 {
        let (status, _) = post(14511, "sk-limited");
        // The upstream is unreachable in this test (no server on :8199), so
        // an admitted request fails downstream with 502 — anything other
        // than 429 proves the limiter let it through.
        assert_ne!(status, 429, "request {i} of 3 should have been admitted");
    }

    let (status, retry_after) = post(14511, "sk-limited");
    assert_eq!(
        status, 429,
        "the 4th request within the window must be rejected"
    );
    let retry_after: u64 = retry_after
        .expect("a 429 must carry Retry-After")
        .parse()
        .expect("Retry-After must be an integer number of seconds");
    assert!(
        retry_after > 0 && retry_after <= 60,
        "Retry-After should be a sane wait within one window, got {retry_after}"
    );
}

/// A key with no `limits` entry at all must never be rejected by the
/// limiter, no matter how many requests it sends — the same "unlimited, not
/// zero" property `crate::limiter`'s unit tests pin, proven here through the
/// real HTTP path.
#[test]
fn a_key_with_no_configured_limit_is_never_429d() {
    let _p = start(CONFIG, 14512);
    for i in 0..10 {
        let (status, _) = post(14512, "sk-unlimited");
        assert_ne!(
            status, 429,
            "unlimited request {i} must never be rate limited"
        );
    }
}
