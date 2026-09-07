//! Reading an inference engine's own view of its load.
//!
//! Outside `control` on purpose: both planes need it. The control plane reads
//! it once a minute for the providers page, and every proxy reads it every
//! couple of seconds for `max_inflight_per_backend` — see
//! `crate::engine_scrape`.

use crate::upstream::Upstream;

/// What an engine says about its own load, read from its Prometheus endpoint.
///
/// Every engine here already serves `/metrics`: vLLM and SGLang both do, and
/// it is one text format rather than an endpoint per vendor. The metric
/// *names* differ, which is the only engine-specific knowledge in this file
/// and is confined to the name lists below.
///
/// Why this is worth having when the proxy already counts in-flight requests:
/// its counter is per replica, so two proxies each admit up to the configured
/// limit and `max_inflight_per_backend` effectively doubles. The engine counts
/// once, for everybody — including traffic that did not come through FastLLM
/// at all.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EngineLoad {
    /// Requests the engine is working on now.
    pub running: u32,
    /// Requests it has accepted and not started.
    pub waiting: u32,
    /// KV cache in use, 0.0–1.0. `None` when the engine does not report it.
    pub kv_cache: Option<f32>,
}

/// The families worth reading, newest name first.
///
/// vLLM renamed `gpu_cache_usage_perc` to `kv_cache_usage_perc`, and a
/// deployment mid-upgrade runs both, so each row lists the alternatives rather
/// than pinning one. A name that matches nothing leaves its field at zero,
/// which reads as "no load" — the safe direction, since it can only make a
/// backend look more available than it is to a *display*, and routing checks
/// freshness before believing any of this.
const RUNNING: &[&str] = &["vllm:num_requests_running", "sglang:num_running_reqs"];
const WAITING: &[&str] = &["vllm:num_requests_waiting", "sglang:num_queue_reqs"];
const KV_CACHE: &[&str] = &[
    "vllm:kv_cache_usage_perc",
    "vllm:gpu_cache_usage_perc",
    "sglang:token_usage",
];

/// Sum every sample of the first family that appears.
///
/// Summed, not taken once: a server with tensor or data parallelism reports
/// one sample per engine (`engine="0"`, `engine="1"`), and the load that
/// matters is the whole server's. A gauge like cache usage is averaged
/// instead — adding two 50% caches into 100% would be nonsense.
fn read_family(body: &str, names: &[&str], average: bool) -> Option<f64> {
    for name in names {
        let mut total = 0.0;
        let mut count = 0u32;
        for line in body.lines() {
            let Some(rest) = line.strip_prefix(name) else {
                continue;
            };
            // `name{labels} value` or `name value` — but not `name_total`,
            // which is a different family that happens to share a prefix.
            let rest = match rest.chars().next() {
                Some('{') => match rest.split_once('}') {
                    Some((_, after)) => after,
                    None => continue,
                },
                Some(' ') => rest,
                _ => continue,
            };
            if let Ok(v) = rest.trim().parse::<f64>() {
                if v.is_finite() {
                    total += v;
                    count += 1;
                }
            }
        }
        if count > 0 {
            return Some(if average { total / count as f64 } else { total });
        }
    }
    None
}

/// Ask an engine what it is currently doing.
///
/// `/metrics` sits at the server root, not under the `/v1` an `api_base`
/// ends with — so the path is derived from the origin rather than appended.
pub async fn engine_load(client: &Upstream, api_base: &str) -> anyhow::Result<EngineLoad> {
    use http_body_util::BodyExt as _;
    let origin = metrics_origin(api_base);
    let url = format!("{origin}/metrics");
    let req = hyper::Request::builder()
        .method("GET")
        .uri(&url)
        .header(hyper::header::USER_AGENT, "fastllm-proxy")
        .body(http_body_util::Full::new(bytes::Bytes::new()))?;
    // Shorter than the model-list probe: this is a nicety, and a slow answer
    // is worth less than a prompt sweep.
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), client.request(req))
        .await
        .map_err(|_| anyhow::anyhow!("{url} timed out"))??;
    if !resp.status().is_success() {
        anyhow::bail!("{url} answered {}", resp.status());
    }
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("reading {url}: {e}"))?
        .to_bytes();
    let body = std::str::from_utf8(&body)?;
    Ok(EngineLoad {
        running: read_family(body, RUNNING, false).unwrap_or(0.0).max(0.0) as u32,
        waiting: read_family(body, WAITING, false).unwrap_or(0.0).max(0.0) as u32,
        kv_cache: read_family(body, KV_CACHE, true).map(|v| v.clamp(0.0, 1.0) as f32),
    })
}

/// `http://host:8000/v1` -> `http://host:8000`.
///
/// Only the `/v1` comes off, and nothing cleverer is attempted. A vendor that
/// mounts the OpenAI routes deeper — Groq's `/openai/v1` — is left pointing at
/// `/openai/metrics`, which answers nothing; that is the right outcome, since
/// a hosted provider does not expose its scheduler's metrics to us anyway. The
/// engines this is for put `/metrics` exactly one level above `/v1`.
fn metrics_origin(api_base: &str) -> String {
    let trimmed = api_base.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

#[cfg(test)]
mod tests {
    /// Parsed against the real thing: these lines are copied from the vLLM on
    /// 192.168.10.245:8000, labels and all.
    #[test]
    fn engine_load_is_read_out_of_prometheus_text() {
        let body = "\
# HELP vllm:num_requests_running Number of requests currently running.
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine=\"0\",model_name=\"nvidia/Qwen3.6-35B\"} 3.0
vllm:num_requests_waiting{engine=\"0\",model_name=\"nvidia/Qwen3.6-35B\"} 2.0
vllm:num_requests_waiting_by_reason{engine=\"0\",model_name=\"x\",reason=\"capacity\"} 9.0
vllm:kv_cache_usage_perc{engine=\"0\",model_name=\"nvidia/Qwen3.6-35B\"} 0.42
";
        assert_eq!(super::read_family(body, super::RUNNING, false), Some(3.0));
        // `_by_reason` shares the `num_requests_waiting` prefix and is a
        // different family; counting it would triple the queue depth.
        assert_eq!(super::read_family(body, super::WAITING, false), Some(2.0));
        assert_eq!(super::read_family(body, super::KV_CACHE, true), Some(0.42));
    }

    /// Tensor or data parallelism reports one sample per engine.
    #[test]
    fn samples_are_summed_but_a_utilisation_gauge_is_averaged() {
        let body = "\
vllm:num_requests_running{engine=\"0\"} 3.0
vllm:num_requests_running{engine=\"1\"} 4.0
vllm:kv_cache_usage_perc{engine=\"0\"} 0.50
vllm:kv_cache_usage_perc{engine=\"1\"} 0.30
";
        // Seven requests are in flight on that server, not four.
        assert_eq!(super::read_family(body, super::RUNNING, false), Some(7.0));
        // Two caches half and a third full is not a cache at 80%.
        assert_eq!(super::read_family(body, super::KV_CACHE, true), Some(0.40));
    }

    /// SGLang names them differently, and a deployment may run either.
    #[test]
    fn sglangs_names_are_read_the_same_way() {
        let body = "sglang:num_running_reqs 5.0\nsglang:num_queue_reqs 1.0\n";
        assert_eq!(super::read_family(body, super::RUNNING, false), Some(5.0));
        assert_eq!(super::read_family(body, super::WAITING, false), Some(1.0));
        assert_eq!(super::read_family(body, super::KV_CACHE, true), None);
    }

    /// `/metrics` is at the server root, not under the `/v1` an api_base ends
    /// with — appending would ask for `/v1/metrics`, which is a 404.
    #[test]
    fn metrics_live_at_the_origin_not_under_v1() {
        assert_eq!(
            super::metrics_origin("http://host:8000/v1"),
            "http://host:8000"
        );
        // A vendor that mounts the routes deeper is left alone rather than
        // guessed at; it has no scheduler metrics for us either way.
        assert_eq!(
            super::metrics_origin("https://api.groq.com/openai/v1"),
            "https://api.groq.com/openai"
        );
        // A gateway with a path prefix keeps it.
        assert_eq!(
            super::metrics_origin("http://gw/engines/a/v1"),
            "http://gw/engines/a"
        );
    }
}
