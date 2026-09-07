//! Per-proxy scraping of engine load, for `max_inflight_per_backend`.
//!
//! The control plane already reads `/metrics` during its provider sweep, but
//! that runs once a minute and lands in Postgres — fine for the providers page,
//! useless for a routing decision. So each proxy reads its own backends here,
//! every couple of seconds, into the same `Backend` the router already holds.
//! The request path still touches nothing but an atomic; see
//! `Backend::inflight_for_limit`.
//!
//! Only self-hosted engines have this to offer, and which ones those are is
//! detected rather than configured. A backend that answers with something that
//! is not engine metrics is asked once more and then never again, so a
//! deployment full of hosted providers pays two requests per provider for the
//! life of the process and nothing after that. A backend that does not answer
//! at all keeps being retried, because an engine part way through loading a
//! model is indistinguishable from one that is down.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::debug;

use crate::engine_metrics::ProbeError;
use crate::registry::{now_ms, BackendUid};
use crate::state::AppState;

/// Ticks a backend sits out after a probe that produced no reading. At the
/// default two-second interval that is five minutes — long enough that a
/// backend which will never answer is not being polled, short enough that
/// standing an engine up is noticed without a restart.
const RETRY_AFTER_TICKS: u64 = 150;

/// What is known about one backend's `/metrics`.
enum Detected {
    /// It answered with something that is not engine metrics, and it was
    /// healthy at the time — so it is serving, and simply has none. Never
    /// probed again while this configuration lasts.
    ///
    /// Health is what makes this safe to settle on. A `Backend` starts
    /// optimistically healthy and a vLLM still loading a model may serve a
    /// bare `/metrics` before its scheduler publishes anything, so the first
    /// such answer only earns a retry; it takes a second one, five minutes
    /// later, by which time the health sweep has had its say.
    Never,
    /// Ask again on this tick. `answered` records whether the last probe got
    /// a reply that simply was not engine metrics, which is what the second
    /// such reply is counted against — a backend that was merely unreachable
    /// starts that count over rather than settling on the strength of one
    /// answer.
    RetryAt { at: u64, answered: bool },
}

pub fn spawn(state: Arc<AppState>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Keyed by uid, which identifies a backend *configuration*: editing an
        // endpoint mints a new one, so a provider that gains an engine is
        // detected afresh rather than inheriting a verdict about the old
        // address. Held in the task because it is a hint — losing it on
        // restart costs one more round of probes.
        let mut known: HashMap<BackendUid, Detected> = HashMap::new();
        let mut tick: u64 = 0;
        loop {
            ticker.tick().await;
            tick += 1;
            sweep(&state, tick, &mut known).await;
        }
    });
}

async fn sweep(state: &Arc<AppState>, tick: u64, known: &mut HashMap<BackendUid, Detected>) {
    let backends: Vec<_> = state
        .registry
        .load()
        .backends()
        .iter()
        .filter(|b| match known.get(&b.uid) {
            Some(Detected::Never) => false,
            Some(Detected::RetryAt { at, .. }) => tick >= *at,
            None => true,
        })
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
                known.remove(&backend.uid);
                backend.record_engine_inflight((load.running + load.waiting) as usize, now_ms());
            }
            // Answered, and it is not an engine. Settle only on the second
            // such answer from a backend that is healthy — see `Detected`.
            Err(e @ ProbeError::NotAnEngine(_)) => {
                let asked_before = matches!(
                    known.get(&backend.uid),
                    Some(Detected::RetryAt { answered: true, .. })
                );
                if asked_before && backend.is_healthy() {
                    known.insert(backend.uid, Detected::Never);
                    debug!(backend = %backend.api_base, error = %e, "no engine metrics; will not ask again");
                } else {
                    known.insert(
                        backend.uid,
                        Detected::RetryAt {
                            at: tick + RETRY_AFTER_TICKS,
                            answered: true,
                        },
                    );
                    debug!(backend = %backend.api_base, error = %e, "no engine metrics; asking once more later");
                }
            }
            // Nothing answered. Says nothing about whether this backend has
            // metrics, so it never settles — only backs off.
            Err(e) => {
                if known.get(&backend.uid).is_none() {
                    debug!(backend = %backend.api_base, error = %e, "engine metrics unreachable; backing off");
                }
                known.insert(
                    backend.uid,
                    Detected::RetryAt {
                        at: tick + RETRY_AFTER_TICKS,
                        answered: false,
                    },
                );
            }
        }
    }
}
