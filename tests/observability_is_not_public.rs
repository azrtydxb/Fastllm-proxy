//! `/health`, `/healthz` and `/metrics` against a real spawned binary.
//!
//! These three are the only routes that answer before authorisation, because a
//! Kubernetes probe carries no credential. That made them the deployment's
//! blind spot the moment the gateway became reachable from the internet: the
//! detailed body names every backend's `api_base` — the internal addressing —
//! along with the model inventory, which third-party providers are in use and
//! live traffic counts, and `/metrics` is 60KB of the same with latency
//! histograms attached.
//!
//! So the split these tests pin is: the *verdict* is public, the *detail* is
//! not. The status code must not depend on the caller, or a probe would start
//! failing the moment a key rotated.

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

fn start(port: u16) -> Proc {
    let path = std::env::temp_dir().join(format!("obs-{port}.yaml"));
    std::fs::write(&path, CONFIG).unwrap();
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
        // Any HTTP answer proves the listener is up, 503 included — the
        // backend below is deliberately dead, so 503 is this config's healthy
        // steady state.
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

/// One backend, pointed at a port nothing listens on. Its address is the thing
/// that must not leak, so it is written here to be searched for by value.
const BACKEND_API_BASE: &str = "http://127.0.0.1:8199/v1";

const CONFIG: &str = "\
model_list:
  - model_name: secret-model
    litellm_params: { api_base: http://127.0.0.1:8199/v1 }
auth:
  keys:
    - key: sk-valid
      name: someone
      models: ['*']
";

/// `(status, body)` — a non-2xx is an outcome here, not a failure.
fn get(port: u16, path: &str, key: Option<&str>) -> (u16, String) {
    let mut req = ureq::get(&format!("http://127.0.0.1:{port}{path}"));
    if let Some(k) = key {
        req = req.set("authorization", &format!("Bearer {k}"));
    }
    match req.call() {
        Ok(r) => {
            let code = r.status();
            let mut buf = String::new();
            let _ = r.into_reader().read_to_string(&mut buf);
            (code, buf)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut buf = String::new();
            let _ = resp.into_reader().read_to_string(&mut buf);
            (code, buf)
        }
        Err(e) => panic!("request to {path} failed: {e}"),
    }
}

#[test]
fn health_tells_an_anonymous_caller_nothing_about_the_deployment() {
    let _p = start(14611);

    for path in ["/health", "/healthz"] {
        let (code, body) = get(14611, path, None);
        assert!(
            code == 200 || code == 503,
            "{path} must still answer a probe, got {code}"
        );

        // The leak, named precisely: the internal address, the inventory, and
        // the counts that tell an observer how much traffic this thing carries.
        assert!(
            !body.contains(BACKEND_API_BASE),
            "{path} leaked a backend address to an anonymous caller: {body}"
        );
        for forbidden in [
            "api_base",
            "backends",
            "models",
            "keys",
            "requests_total",
            "snapshot_version",
            "policy",
        ] {
            assert!(
                !body.contains(forbidden),
                "{path} leaked {forbidden:?} to an anonymous caller: {body}"
            );
        }
        // Still useful: a probe needs the verdict, and so does a human curling it.
        assert!(
            body.contains("status"),
            "{path} must still report status: {body}"
        );
    }
}

/// The probe contract. If the code moved with the caller's credential, a key
/// rotation would read as an outage and Kubernetes would restart healthy pods.
#[test]
fn the_status_code_does_not_depend_on_who_is_asking() {
    let _p = start(14612);

    let (anonymous, _) = get(14612, "/health", None);
    let (authenticated, _) = get(14612, "/health", Some("sk-valid"));
    assert_eq!(
        anonymous, authenticated,
        "a probe's verdict must not depend on whether it holds a key"
    );
}

#[test]
fn a_valid_key_still_gets_the_detail() {
    let _p = start(14613);

    let (code, body) = get(14613, "/health", Some("sk-valid"));
    assert!(code == 200 || code == 503, "unexpected status {code}");
    assert!(
        body.contains(BACKEND_API_BASE),
        "an authenticated caller should still see backend detail: {body}"
    );
    assert!(body.contains("snapshot_version"), "body was {body}");
}

/// `/metrics` is all detail and has no reduced form, so it is refused rather
/// than emptied.
#[test]
fn metrics_is_refused_without_a_key_and_served_with_one() {
    let _p = start(14614);

    let (code, body) = get(14614, "/metrics", None);
    assert_eq!(code, 401, "/metrics must not be public: {body}");
    assert!(
        !body.contains(BACKEND_API_BASE),
        "even the refusal must not name a backend: {body}"
    );

    let (code, body) = get(14614, "/metrics", Some("sk-valid"));
    assert_eq!(code, 200, "a valid key must still be able to scrape");
    assert!(
        body.contains("fastllm_requests_total"),
        "expected Prometheus text, got {body}"
    );
}

/// A bad key is not a key. Worth its own case because the predicate deciding
/// this is separate from `authorize` and could drift from it.
#[test]
fn a_bogus_key_earns_nothing() {
    let _p = start(14615);

    let (_, body) = get(14615, "/health", Some("sk-nonsense"));
    assert!(
        !body.contains(BACKEND_API_BASE),
        "a bogus key must be treated as no key: {body}"
    );
    assert_eq!(get(14615, "/metrics", Some("sk-nonsense")).0, 401);
}

/// `/livez` is the endpoint the operator's liveness probe asks, so it must
/// answer 200 with no credential and stay 200 when every backend is down.
///
/// Liveness must not track backend health: `/health` goes 503 once its probes
/// have failed, which is correct for readiness and would have the kubelet
/// restart every pod during a backend outage. That distinction is not asserted
/// here because it depends on probe timing; what is asserted is the half that
/// does not -- `/livez` needs no key and says nothing. Gating `/metrics`, which
/// the operator had been using for liveness, restart-looped every pod on a 401.
#[test]
fn livez_is_open_and_stays_up_when_every_backend_is_down() {
    let _p = start(14616);

    let (code, body) = get(14616, "/livez", None);
    assert_eq!(
        code, 200,
        "liveness must not depend on backend health: {body}"
    );
    assert!(body.contains("alive"), "body was {body}");

    // And it discloses nothing, like the other open route.
    assert!(!body.contains(BACKEND_API_BASE), "body was {body}");
    for forbidden in ["api_base", "backends", "models"] {
        assert!(
            !body.contains(forbidden),
            "/livez leaked {forbidden}: {body}"
        );
    }
}
