//! End-to-end authorisation against a real spawned binary.
//!
//! Unit tests exercise `authorize`/`may_invoke` directly; this proves the
//! ordering holds through the actual HTTP path a client sees, with a real
//! bind, a real listener and a real config file on disk. `--role proxy` in
//! `File` mode is what makes this tractable without a database: the whole
//! system runs from one YAML file in one process.

use std::io::Read;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        // Best-effort: if the test already failed and panicked, this still
        // runs during unwind, so a failing assertion never leaks a listener
        // that would collide with the next run on the same port.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns the real binary against a freshly written config file and blocks
/// until it answers `/health`, rather than sleeping a fixed duration — ports
/// can be slow to bind under load, and a fixed sleep is exactly what makes a
/// CI run flaky.
fn start(config: &str, port: u16) -> Proc {
    let path = std::env::temp_dir().join(format!("rbac-{port}.yaml"));
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
        if ureq::get(&format!("http://127.0.0.1:{port}/health"))
            .call()
            .is_ok()
        {
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
  - model_name: allowed-model
    litellm_params: { api_base: http://127.0.0.1:8199/v1 }
  - model_name: other-model
    litellm_params: { api_base: http://127.0.0.1:8199/v1 }
auth:
  keys:
    - key: sk-narrow
      name: narrow
      models: [allowed-model]
    - key: sk-wide
      name: wide
      models: ['*']
    - key: sk-expired
      name: expired
      models: ['*']
      expires_at: 2020-01-01T00:00:00Z
";

fn post(port: u16, key: Option<&str>, model: &str) -> u16 {
    let mut req = ureq::post(&format!("http://127.0.0.1:{port}/v1/chat/completions"));
    if let Some(k) = key {
        req = req.set("authorization", &format!("Bearer {k}"));
    }
    match req.send_json(ureq::json!({ "model": model, "messages": [] })) {
        Ok(r) => r.status(),
        Err(ureq::Error::Status(code, resp)) => {
            // Drain the body so the connection can be reused by the next
            // request on this same short-lived agent; otherwise ureq's
            // default agent can stall waiting on a half-read response.
            let mut buf = String::new();
            let _ = resp.into_reader().read_to_string(&mut buf);
            code
        }
        Err(e) => panic!("request failed: {e}"),
    }
}

/// The assertion that matters most: authenticate, then resolve the model,
/// then authorise. Getting 404 and 403 the wrong way round either leaks that
/// a model exists to a caller who should never have known, or hides a real
/// "you're not allowed to use that" behind a misleading "no such model".
#[test]
fn authorisation_allows_denies_and_expires() {
    let _p = start(CONFIG, 14411);

    assert_eq!(post(14411, None, "allowed-model"), 401, "no key");
    assert_eq!(
        post(14411, Some("sk-bogus"), "allowed-model"),
        401,
        "unknown key"
    );
    assert_eq!(
        post(14411, Some("sk-expired"), "allowed-model"),
        401,
        "expired key"
    );
    // The discriminator this whole test exists for: these two must land on
    // opposite sides of authorisation. A narrow key against a model that
    // EXISTS but isn't granted is 403 (the model resolved; the grant check
    // rejected it). The same key against a model that does NOT exist is 404
    // (resolution fails before authorisation is even consulted) — a caller
    // must not be able to use "403 vs 404" to enumerate which models exist
    // by testing keys it isn't granted for. Keep both assertions here,
    // together: deleting one leaves the other looking sufficient when it
    // proves nothing about ordering on its own.
    assert_eq!(
        post(14411, Some("sk-narrow"), "other-model"),
        403,
        "ungranted model that exists"
    );
    assert_eq!(
        post(14411, Some("sk-narrow"), "no-such-model"),
        404,
        "ungranted key against a model that does not exist at all"
    );
    // Granted: reaches routing and fails at the (absent) upstream, not at
    // authz — anything but 403 proves the grant was honoured.
    assert_ne!(
        post(14411, Some("sk-narrow"), "allowed-model"),
        403,
        "granted model must not be forbidden"
    );
    assert_ne!(
        post(14411, Some("sk-wide"), "other-model"),
        403,
        "wildcard grant"
    );
}

#[test]
fn an_unknown_model_is_404_for_an_authorised_caller() {
    let _p = start(CONFIG, 14412);
    assert_eq!(post(14412, Some("sk-wide"), "no-such-model"), 404);
}
