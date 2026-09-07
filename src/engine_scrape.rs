//! Per-proxy scraping of engine load, for `max_inflight_per_backend`.
//!
//! The control plane already reads `/metrics` during its provider sweep, but
//! that runs once a minute and lands in Postgres — fine for the providers page,
//! useless for a routing decision. So each proxy reads its own backends here,
//! every couple of seconds, into the same `Backend` the router already holds.
//! The request path still touches nothing but an atomic; see
//! `Backend::inflight_for_limit`.
//!
//! Only self-hosted engines answer this. Rather than teach the snapshot which
//! backends are engines, the scraper finds out by asking and remembers: a
//! backend whose `/metrics` does not parse is put aside and retried rarely, so
//! a deployment that adds an engine later is picked up without a restart and a
//! hosted provider costs one request every few minutes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::debug;

use crate::registry::{now_ms, BackendUid};
use crate::state::AppState;

/// Ticks a backend with no usable `/metrics` sits out before being tried
/// again. At the default two-second interval that is five minutes — long
/// enough that a hosted provider is not being polled, short enough that
/// standing an engine up is noticed without a restart.
const RETRY_AFTER_TICKS: u64 = 150;

pub fn spawn(state: Arc<AppState>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Which backends answered, and for the ones that did not, the tick
        // they may be tried again on. Local to the task: it is a hint, and
        // losing it on restart costs one wasted round of probes.
        let mut quiet: HashMap<BackendUid, u64> = HashMap::new();
        let mut tick: u64 = 0;
        loop {
            ticker.tick().await;
            tick += 1;
            sweep(&state, tick, &mut quiet).await;
        }
    });
}

async fn sweep(state: &Arc<AppState>, tick: u64, quiet: &mut HashMap<BackendUid, u64>) {
    let backends: Vec<_> = state
        .registry
        .load()
        .backends()
        .iter()
        .filter(|b| quiet.get(&b.uid).is_none_or(|&until| tick >= until))
        .cloned()
        .collect();

    let mut tasks = JoinSet::new();
    for backend in backends {
        let state = Arc::clone(state);
        tasks.spawn(async move {
            let load = crate::engine_metrics::engine_load(&state.client, &backend.api_base).await;
            (backend, load)
        });
    }

    while let Some(Ok((backend, load))) = tasks.join_next().await {
        match load {
            // `running + waiting`: a request the engine has accepted but not
            // started is still a request this backend owes an answer for, and
            // a ceiling that ignored the queue would keep feeding a node whose
            // queue is the reason it is slow.
            Ok(load) => {
                backend.record_engine_inflight((load.running + load.waiting) as usize, now_ms());
                quiet.remove(&backend.uid);
            }
            Err(e) => {
                if quiet
                    .insert(backend.uid, tick + RETRY_AFTER_TICKS)
                    .is_none()
                {
                    debug!(backend = %backend.api_base, error = %e, "no engine metrics; backing off");
                }
            }
        }
    }
}
