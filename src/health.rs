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

use http_body_util::{BodyExt, Full};
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

            let outcome = tokio::time::timeout(probe_timeout, state.client.request(request)).await;

            let ok = match outcome {
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    // Drain so the pooled connection is reusable next sweep.
                    let _ = resp.into_body().collect().await;
                    status.is_success()
                }
                Ok(Err(_)) | Err(_) => false,
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
