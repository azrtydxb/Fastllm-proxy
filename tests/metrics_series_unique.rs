//! Every series in `/metrics` must be unique, against a real spawned binary.
//!
//! Several providers share one `api_base` — every OpenRouter model is
//! `https://openrouter.ai/api/v1` — so a per-backend series labelled by
//! `api_base` alone was emitted once per backend with identical labels. The
//! exposition format allows one sample per series per scrape; Prometheus kept
//! the first and dropped the rest, raising PrometheusDuplicateTimestamps and
//! silently merging five backends' health, request, error and latency numbers
//! into whichever came first. Four metrics had this shape while their siblings
//! (`fastllm_backend_inflight`, `…_ejections_total`) already carried `model`.
//!
//! The assertion is on the whole exposition rather than those four names, so a
//! new per-backend metric that forgets the label fails here too.

use std::collections::HashMap;
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

/// Two backends on the SAME `api_base`, differing only in the upstream model —
/// the OpenRouter shape. Nothing listens on the port; the backends being down
/// is fine, their series are rendered either way.
const CONFIG: &str = "\
model_list:
  - model_name: shared-a
    litellm_params: { model: openai/vendor/model-a, api_base: http://127.0.0.1:8197/v1 }
  - model_name: shared-b
    litellm_params: { model: openai/vendor/model-b, api_base: http://127.0.0.1:8197/v1 }
auth:
  keys:
    - key: sk-valid
      name: someone
      models: ['*']
";

const PORT: u16 = 14941;

fn start() -> Proc {
    let path = std::env::temp_dir().join(format!("metrics-unique-{PORT}.yaml"));
    std::fs::write(&path, CONFIG).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_fastllm-proxy"))
        .args([
            "--config",
            path.to_str().unwrap(),
            "--port",
            &PORT.to_string(),
            "--role",
            "proxy",
        ])
        .spawn()
        .expect("failed to spawn fastllm-proxy");
    let proc = Proc(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Any HTTP answer proves the listener is up; 503 is this config's
        // steady state because both backends are dead.
        if !matches!(
            ureq::get(&format!("http://127.0.0.1:{PORT}/health")).call(),
            Err(ureq::Error::Transport(_))
        ) {
            return proc;
        }
        if Instant::now() >= deadline {
            panic!("fastllm-proxy on port {PORT} did not answer /health within 10s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn scrape() -> String {
    let resp = ureq::get(&format!("http://127.0.0.1:{PORT}/metrics"))
        .set("authorization", "Bearer sk-valid")
        .call()
        .expect("scrape /metrics");
    let mut body = String::new();
    resp.into_reader().read_to_string(&mut body).unwrap();
    body
}

#[test]
fn backends_sharing_an_api_base_do_not_collapse_into_one_series() {
    let _p = start();
    let body = scrape();

    // A sample line is `name{labels} value`; the series identity is
    // everything before the last space.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for line in body
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let series = line.rsplit_once(' ').map_or(line, |(s, _)| s);
        *seen.entry(series).or_default() += 1;
    }
    let mut dups: Vec<_> = seen.into_iter().filter(|(_, n)| *n > 1).collect();
    dups.sort();
    assert!(
        dups.is_empty(),
        "/metrics repeats {} series; Prometheus keeps the first sample of each and \
         drops the rest:\n{}",
        dups.len(),
        dups.iter()
            .map(|(s, n)| format!("  {s}  ×{n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // And the two backends are told apart by name, not merely de-duplicated.
    for model in ["vendor/model-a", "vendor/model-b"] {
        for metric in [
            "fastllm_backend_healthy",
            "fastllm_backend_requests_total",
            "fastllm_backend_errors_total",
            "fastllm_backend_duration_seconds_count",
        ] {
            let want =
                format!("{metric}{{api_base=\"http://127.0.0.1:8197/v1\",model=\"{model}\"}}");
            assert!(
                body.contains(&want),
                "/metrics has no {want}; per-backend series must carry the upstream model"
            );
        }
    }
}
