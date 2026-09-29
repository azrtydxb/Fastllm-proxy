//! Background health probing.
//!
//! Probes `GET {api_base}/models` on every known backend at a fixed interval.
//! A backend has to fail `unhealthy_after` consecutive probes before it leaves
//! rotation, so one dropped packet during a busy prefill does not evict a node
//! that is merely working hard.
//!
//! One path serves every protocol: OpenAI-compatible upstreams, Anthropic
//! (`GET /v1/models`, same shape) and Gemini (`GET /v1beta/models`) all expose
//! a model listing at `{api_base}/models`, so nothing here needs to know which
//! wire format the backend speaks.

use http_body_util::Full;
use hyper::{Method, Request};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::state::AppState;

pub fn spawn(state: Arc<AppState>, interval: Duration, probe_timeout: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            sweep(&state, probe_timeout).await;
        }
    });
}

/// The context window an engine's `/models` reports, if it does.
///
/// The **smallest** across what the endpoint serves: one engine may host
/// several models and the figure has to hold for whichever answers.
///
/// Anything unparseable is `None` rather than an error. A hosted provider
/// answers this probe with a model list that has no such field, and that is
/// not a fault — it is a provider that does not publish the number.
fn context_length_of(body: &[u8]) -> Option<u64> {
    let parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    parsed
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("max_model_len")?.as_u64())
        .filter(|len| *len > 0)
        .min()
}

async fn sweep(state: &Arc<AppState>, probe_timeout: Duration) {
    let backends: Vec<_> = state.registry.load().backends().to_vec();
    let mut tasks = JoinSet::new();

    for backend in backends {
        let state = Arc::clone(state);
        tasks.spawn(async move {
            let url = backend.url_for("/models");
            let mut builder = Request::builder().method(Method::GET).uri(&url);
            // The probe has to authenticate, or a backend that requires a key
            // is permanently unhealthy: a self-hosted vLLM answers `/models`
            // to anyone, but every hosted provider — OpenRouter, Anthropic,
            // Gemini, Groq — answers 401, which this sweep reads as "down"
            // and takes out of rotation while it is serving perfectly well.
            if let Some(headers) = builder.headers_mut() {
                for (name, value) in &backend.headers {
                    headers.insert(name.clone(), value.clone());
                }
            }
            let request = builder
                .body(Full::default())
                .expect("probe URL was validated at config load");

            // Headers *and* body under the one deadline. With a timeout around
            // the request alone the body read was unbounded, and one backend
            // that stalled mid-body would stop this sweep for every backend,
            // forever. Both proxies were once found with this loop stopped and
            // their scrapers still running -- so an ejected backend had nothing
            // left that could bring it back. See `Upstream::fetch`.
            let ok = match state.client.fetch(request, probe_timeout).await {
                Ok(fetched) => {
                    // This is the engine's own `/models`, and it carries the
                    // context window. Keeping it current costs nothing here and
                    // cannot go stale, which a figure stored in the database
                    // can -- `--max-model-len` is chosen at engine start, and a
                    // restart changes it without touching a row.
                    if fetched.status.is_success() {
                        if let Some(len) = context_length_of(&fetched.body) {
                            backend.record_context_length(len);
                        }
                    }
                    fetched.status.is_success()
                }
                Err(_) => false,
            };

            if ok {
                if backend.mark_probe_ok() {
                    info!(backend = %backend.api_base, "backend healthy, back in rotation");
                }
            } else if backend.mark_probe_failed(state.unhealthy_after) {
                warn!(
                    backend = %backend.api_base,
                    "backend failed {} consecutive probes, out of rotation",
                    state.unhealthy_after
                );
            }
        });
    }

    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod context_length_tests {
    use super::context_length_of;

    /// vLLM's own `/models`, which the health probe already fetches and used
    /// to discard. Reading it is what keeps the advertised window matching
    /// the engine after a restart changes `--max-model-len`.
    #[test]
    fn it_reads_what_vllm_reports() {
        let body = br#"{"object":"list","data":[
            {"id":"nvidia/Qwen3.6-35B-A3B-NVFP4","object":"model","max_model_len":262144}
        ]}"#;
        assert_eq!(context_length_of(body), Some(262_144));
    }

    /// One endpoint may serve several models, and the figure has to hold for
    /// whichever answers — so the smallest, not the first or the largest.
    #[test]
    fn several_models_report_the_smallest() {
        let body = br#"{"data":[
            {"id":"big","max_model_len":262144},
            {"id":"small","max_model_len":8192}
        ]}"#;
        assert_eq!(context_length_of(body), Some(8_192));
    }

    /// A hosted provider answers this probe with a list that has no such
    /// field. That is not a fault, it is a provider that does not publish the
    /// number, and it must leave the stored value alone rather than zeroing
    /// it.
    #[test]
    fn a_provider_that_does_not_publish_it_reports_nothing() {
        assert_eq!(
            context_length_of(br#"{"data":[{"id":"gpt-5","object":"model"}]}"#),
            None
        );
        assert_eq!(context_length_of(b"not json at all"), None);
        assert_eq!(context_length_of(br#"{"data":[]}"#), None);
    }

    /// Zero is not a context window. An engine reporting it has said nothing
    /// useful, and counting it would make the model look unroutable for every
    /// prompt.
    #[test]
    fn a_zero_is_not_a_window() {
        assert_eq!(
            context_length_of(br#"{"data":[{"id":"x","max_model_len":0}]}"#),
            None
        );
        assert_eq!(
            context_length_of(
                br#"{"data":[{"id":"x","max_model_len":0},{"id":"y","max_model_len":4096}]}"#
            ),
            Some(4096),
            "a zero beside a real figure is skipped, not taken as the minimum"
        );
    }
}
