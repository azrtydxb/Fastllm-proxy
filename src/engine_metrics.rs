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

/// Why a probe did not produce a reading, and whether asking again could ever
/// change the answer.
///
/// The distinction is the whole of the autodetection. A hosted provider has no
/// scheduler metrics and never will, so it must be asked about roughly twice
/// and then left alone; an engine that is still loading a model is
/// indistinguishable from one that is down, and must keep being asked. Folding
/// both into one error is what would make the scraper either poll every vendor
/// in the catalogue for ever or give up on a backend that was merely starting.
#[derive(Debug)]
pub enum ProbeError {
    /// The address answered, and what came back is not an engine's metrics —
    /// a 404, a page, or a Prometheus body with no scheduler families in it.
    /// Settled: nothing here changes until the backend's configuration does.
    NotAnEngine(String),
    /// Nothing answered, or not in time, or the answer could not be read. Says
    /// nothing about whether this backend has metrics: an engine part way
    /// through loading a model looks exactly like this.
    Unreachable(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnEngine(why) | Self::Unreachable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Ask an engine what it is currently doing.
///
/// `/metrics` sits at the server root, not under the `/v1` an `api_base`
/// ends with — so the path is derived from the origin rather than appended.
///
/// A body with no `RUNNING` family in it is an error, not a reading of zero.
/// That distinction is load-bearing twice over: routing would otherwise treat
/// anything that answers `/metrics` at all as an engine that is permanently
/// idle and never spill off it, and the providers page would show "0 running"
/// for a provider that does not report, which is a different claim from "does
/// not say".
pub async fn engine_load(client: &Upstream, api_base: &str) -> Result<EngineLoad, ProbeError> {
    use http_body_util::BodyExt as _;
    let origin = metrics_origin(api_base);
    let url = format!("{origin}/metrics");
    let req = hyper::Request::builder()
        .method("GET")
        .uri(&url)
        .header(hyper::header::USER_AGENT, "fastllm-proxy")
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        // An address that will not even form a request will not form one next
        // time either, so this is settled rather than transient.
        .map_err(|e| ProbeError::NotAnEngine(format!("{url} is not a usable address: {e}")))?;
    // Shorter than the model-list probe: this is a nicety, and a slow answer
    // is worth less than a prompt sweep.
    let resp =
        match tokio::time::timeout(std::time::Duration::from_secs(5), client.request(req)).await {
            Err(_) => return Err(ProbeError::Unreachable(format!("{url} timed out"))),
            Ok(Err(e)) => return Err(ProbeError::Unreachable(format!("{url}: {e}"))),
            Ok(Ok(resp)) => resp,
        };
    if !resp.status().is_success() {
        return Err(ProbeError::NotAnEngine(format!(
            "{url} answered {}",
            resp.status()
        )));
    }
    let body = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| ProbeError::Unreachable(format!("reading {url}: {e}")))?
        .to_bytes();
    let body = std::str::from_utf8(&body)
        .map_err(|_| ProbeError::NotAnEngine(format!("{url} did not answer with text")))?;
    // `RUNNING` and nothing else decides whether this is an engine: it is the
    // number routing actually consults, so a body that lacks it has nothing to
    // offer even if it is Prometheus text from something else entirely.
    let Some(running) = read_family(body, RUNNING, false) else {
        return Err(ProbeError::NotAnEngine(format!(
            "{url} reports no engine metrics"
        )));
    };
    Ok(EngineLoad {
        running: running.max(0.0) as u32,
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
    use super::{engine_load, ProbeError};

    /// A tiny server that answers every request with one canned body, so the
    /// classification below is exercised over real HTTP rather than against a
    /// hand-built response.
    fn stub(status: &'static str, body: &'static str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                use std::io::{Read as _, Write as _};
                // Drain the request first: closing on unread bytes sends an
                // RST and the client reports a transport error instead of the
                // response we wrote.
                let mut request = Vec::new();
                let mut buf = [0u8; 512];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
        });
        format!("http://{addr}/v1")
    }

    fn client() -> crate::upstream::Upstream {
        crate::upstream::Upstream::new(
            crate::upstream::Config {
                max_idle_per_host: 2,
                idle_timeout: std::time::Duration::from_secs(5),
                connect_timeout: std::time::Duration::from_secs(5),
            },
            rustls::ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth(),
        )
    }

    /// The reason `running` is not `unwrap_or(0)`: something that answers
    /// `/metrics` without being an engine would otherwise read as an engine
    /// that is permanently idle, and `max_inflight_per_backend` would never
    /// spill off it.
    #[tokio::test]
    async fn prometheus_text_from_something_that_is_not_an_engine_is_not_a_reading_of_zero() {
        let base = stub("200 OK", "process_cpu_seconds_total 1.0\ngo_goroutines 7\n");
        let err = engine_load(&client(), &base).await.unwrap_err();
        assert!(
            matches!(err, ProbeError::NotAnEngine(_)),
            "got {err:?}, which would have been believed as a load"
        );
    }

    /// A hosted provider: the address answers, with a 404. Settled, so the
    /// scraper stops asking rather than polling every vendor for ever.
    #[tokio::test]
    async fn a_404_settles_rather_than_being_retried_for_ever() {
        let base = stub("404 Not Found", "nope");
        let err = engine_load(&client(), &base).await.unwrap_err();
        assert!(matches!(err, ProbeError::NotAnEngine(_)), "got {err:?}");
    }

    /// Nothing listening is *not* settled: an engine part way through loading
    /// a model looks exactly like this, and giving up on it would mean a
    /// restart to get it back.
    #[tokio::test]
    async fn a_dead_address_stays_retryable() {
        // Bound and dropped, so the port is almost certainly free and the
        // connection is refused rather than hanging.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let err = engine_load(&client(), &format!("http://{addr}/v1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProbeError::Unreachable(_)), "got {err:?}");
    }

    /// And the happy path still reads what the DGX vLLM actually serves.
    #[tokio::test]
    async fn a_real_engine_body_is_read() {
        let base = stub(
            "200 OK",
            "vllm:num_requests_running{engine=\"0\"} 3.0\n\
             vllm:num_requests_waiting{engine=\"0\"} 2.0\n\
             vllm:kv_cache_usage_perc{engine=\"0\"} 0.42\n",
        );
        let load = engine_load(&client(), &base).await.unwrap();
        assert_eq!(load.running, 3);
        assert_eq!(load.waiting, 2);
        assert_eq!(load.kv_cache, Some(0.42));
    }

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
