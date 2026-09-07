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
//! detected rather than configured. Every backend is asked; one that does not
//! produce a reading is asked again every `RETRY_EVERY` and given up on after
//! `GIVE_UP_AFTER`, at which point it is dropped from the scrape and costs
//! nothing more. A deployment of hosted providers therefore pays a handful of
//! requests per provider for the life of the process, not a poll every few
//! minutes for ever.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::debug;

use crate::engine_metrics::ProbeError;
use crate::registry::{now_ms, BackendUid};
use crate::state::AppState;

/// How long a backend that produced no reading sits out before being asked
/// again. Long next to the scrape interval: this is detection, not monitoring,
/// and the answer changes at most when someone restarts an engine.
const RETRY_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long the retrying goes on before the backend is assumed not to have
/// metrics at all. Three attempts at the interval above.
///
/// The deadline is what stops a permanently-refused address being polled for
/// the life of the process. It is a wall-clock span rather than a count of
/// ticks because the scrape interval is configurable, and "give up after
/// fifteen minutes" should not become "give up after four" when an operator
/// slows the scrape down.
const GIVE_UP_AFTER: Duration = Duration::from_secs(15 * 60);

/// What is known about one backend's `/metrics`.
enum Detected {
    /// It has none, and the scrape is done with it.
    ///
    /// `recheck_when_healthy` is set only when the verdict came from the
    /// deadline rather than from an answer — nothing ever replied, so the
    /// conclusion is an inference, and an engine that took longer than
    /// `GIVE_UP_AFTER` to load a model would be wrongly written off by it. If
    /// such a backend later passes a health probe it is worth exactly one more
    /// question. A backend that answered and simply had no engine metrics is
    /// never asked again: that verdict came from the endpoint itself.
    None { recheck_when_healthy: bool },
    /// Ask again at `next_ms`. `since_ms` is when the current failing streak
    /// began, which is what `GIVE_UP_AFTER` is measured from, and `answered`
    /// records whether the last probe got a reply that simply was not engine
    /// metrics — a backend that was merely unreachable starts that count over
    /// rather than settling on the strength of one answer.
    Retry {
        next_ms: u64,
        since_ms: u64,
        answered: bool,
    },
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
        loop {
            ticker.tick().await;
            sweep(&state, &mut known).await;
        }
    });
}

async fn sweep(state: &Arc<AppState>, known: &mut HashMap<BackendUid, Detected>) {
    let now = now_ms();
    let backends: Vec<_> = state
        .registry
        .load()
        .backends()
        .iter()
        .filter(|b| match known.get(&b.uid) {
            Some(Detected::None {
                recheck_when_healthy,
            }) => *recheck_when_healthy && b.is_healthy(),
            Some(Detected::Retry { next_ms, .. }) => now >= *next_ms,
            None => true,
        })
        .cloned()
        .collect();

    // Taking the one recheck a revived backend is owed, so a flapping health
    // check cannot turn the verdict back into a poll.
    for b in &backends {
        if let Some(Detected::None {
            recheck_when_healthy,
        }) = known.get_mut(&b.uid)
        {
            *recheck_when_healthy = false;
        }
    }

    let mut tasks = JoinSet::new();
    for backend in backends {
        let state = Arc::clone(state);
        tasks.spawn(async move {
            let load = crate::engine_metrics::engine_load(&state.client, &backend.api_base).await;
            (backend, load)
        });
    }

    while let Some(Ok((backend, load))) = tasks.join_next().await {
        let uid = backend.uid;
        let since_ms = match known.get(&uid) {
            Some(Detected::Retry { since_ms, .. }) => *since_ms,
            _ => now,
        };
        let asked_before = matches!(
            known.get(&uid),
            Some(Detected::Retry { answered: true, .. })
        );

        match load {
            // `running + waiting`: a request the engine has accepted but not
            // started is still a request this backend owes an answer for, and
            // a ceiling that ignored the queue would keep feeding a node whose
            // queue is the reason it is slow.
            Ok(load) => {
                known.remove(&uid);
                backend.record_engine_inflight((load.running + load.waiting) as usize, now_ms());
            }
            Err(e) => {
                // Answered, and it is not an engine: settle on the second such
                // answer from a backend that is serving. Never answered: settle
                // only once the deadline has passed, and mark the verdict as
                // the inference it is.
                let answered = matches!(e, ProbeError::NotAnEngine(_));
                // A second such answer from a backend that is serving settles
                // it early — the endpoint has said its piece and health says
                // it was in a position to. Everything else settles on the
                // deadline, including a backend that keeps answering this way
                // while never passing a health probe, which would otherwise
                // retry for ever waiting for a condition it may never meet.
                let expired = now.saturating_sub(since_ms) >= GIVE_UP_AFTER.as_millis() as u64;
                let settled = if answered && asked_before && backend.is_healthy() {
                    Some(false)
                } else if expired {
                    // The verdict is worth revisiting only when nothing ever
                    // replied, so the conclusion was an inference.
                    Some(!answered)
                } else {
                    None
                };
                match settled {
                    Some(recheck_when_healthy) => {
                        known.insert(
                            uid,
                            Detected::None {
                                recheck_when_healthy,
                            },
                        );
                        debug!(backend = %backend.api_base, error = %e, "no engine metrics; will not ask again");
                    }
                    None => {
                        known.insert(
                            uid,
                            Detected::Retry {
                                next_ms: now + RETRY_EVERY.as_millis() as u64,
                                since_ms,
                                answered,
                            },
                        );
                        debug!(backend = %backend.api_base, error = %e, "no engine metrics; will ask again");
                    }
                }
            }
        }
    }
}
