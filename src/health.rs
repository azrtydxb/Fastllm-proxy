//! Background health probing.
//!
//! Probes `GET {api_base}/models` on every known backend at a fixed interval.
//! A backend has to fail `unhealthy_after` consecutive probes before it leaves
//! rotation, so one dropped packet during a busy prefill does not evict a node
//! that is merely working hard.

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
            let request = Request::builder()
                .method(Method::GET)
                .uri(&url)
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
