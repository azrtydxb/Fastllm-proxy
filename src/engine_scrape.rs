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

/// Only a successful parse of an engine's scheduler families can activate an
/// automatic gate. A vendor 404, non-engine Prometheus body, or failed probe
/// leaves the backend untouched -- so a hosted provider, which never answers
/// with `vllm:`/`sglang:` families, is never gated by accident.
///
/// The capacity policy lives in [`crate::admission::auto_settings`]; this only
/// supplies what the scrape learned and the configured replica count, so the
/// same numbers are unit-testable without a probe.
fn auto_settings_from_probe(
    probe: &Result<crate::engine_metrics::EngineLoad, ProbeError>,
    replicas: u32,
) -> Option<crate::admission::Settings> {
    let Ok(load) = probe else {
        return None;
    };
    Some(crate::admission::auto_settings(load.batch_capacity, replicas))
}

/// Whether a failed probe should put an Auto gate back at its full ceiling
/// right now, or only once the last reading has gone stale.
///
/// `Some(false)` -- the endpoint answered and is not an engine -- is a verdict
/// that needs no waiting. `Some(true)` is a probe that got no usable answer:
/// that can be an engine too busy to serve `/metrics`, the overload the gate is
/// holding back, so the gate waits for the reading to age out first. See
/// [`crate::registry::Backend::restore_auto_admission`] for why the gate is
/// restored rather than removed.
fn restore_only_if_silent(error: &ProbeError) -> bool {
    matches!(error, ProbeError::Unreachable(_))
}

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

pub fn spawn(state: Arc<AppState>, interval: Duration, replicas: u32) {
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
            sweep(&state, &mut known, replicas).await;
        }
    });
}

async fn sweep(state: &Arc<AppState>, known: &mut HashMap<BackendUid, Detected>, replicas: u32) {
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

        if let Some(settings) = auto_settings_from_probe(&load, replicas) {
            backend.enable_auto_admission(settings);
        }

        match load {
            // `running + waiting`: a request the engine has accepted but not
            // started is still a request this backend owes an answer for, and
            // a ceiling that ignored the queue would keep feeding a node whose
            // queue is the reason it is slow.
            Ok(load) => {
                known.remove(&uid);
                let now_ms = now_ms();
                // Stall first, record second: the check compares the scrape
                // against the *previous* stored reading, and recording would
                // overwrite it with the current one.
                let stalled = backend.check_stall(
                    &load,
                    // Ten seconds of silence at the default scrape rate, not
                    // four: a long unchunked prefill or a graph capture
                    // freezes every counter for a few seconds on an engine
                    // that is working, and ejecting it for that is how a busy
                    // GPU got taken out of rotation.
                    state.unhealthy_after.max(5),
                );
                backend.record_engine_inflight(&load, now_ms);
                // The gate reacts to the engine's own queue, and this is where
                // that number arrives. Fed after recording so the gate and the
                // counters describe the same sample.
                if let Some(gate) = backend.admission() {
                    gate.observe(load.waiting);
                }
                if stalled {
                    debug!(
                        backend = %backend.api_base,
                        "engine stalled: token counters unchanged"
                    );
                }
            }
            Err(e) => {
                // The gate recovers from readings of an empty queue, and a
                // failed scrape supplies none -- nor will the next one for five
                // minutes, or ever once the backend is settled. Without this a
                // gate shrunk by overload stays shrunk after the engine
                // restarts or loses `/metrics`.
                backend.restore_auto_admission(now_ms(), restore_only_if_silent(&e));
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

#[cfg(test)]
mod tests {
    use super::{auto_settings_from_probe, restore_only_if_silent};
    use crate::engine_metrics::ProbeError;

    #[test]
    fn engine_capacity_and_provider_errors_select_expected_auto_settings() {
        let measured = Ok(crate::engine_metrics::EngineLoad {
            batch_capacity: Some(8),
            ..Default::default()
        });
        assert_eq!(
            auto_settings_from_probe(&measured, 2).unwrap().max_concurrent,
            4,
            "a small reported batch divides across replicas and lowers the ceiling"
        );
        assert_eq!(
            auto_settings_from_probe(&Ok(crate::engine_metrics::EngineLoad::default()), 2),
            Some(crate::admission::AUTO_FALLBACK),
            "an engine with no advertised capacity gets the measured default"
        );

        // A hosted provider never answers with an engine family, so the probe
        // fails and no gate is installed -- the whole point of "auto".
        let hosted = Err(ProbeError::NotAnEngine("404".into()));
        assert_eq!(auto_settings_from_probe(&hosted, 2), None);
    }

    #[test]
    fn the_configured_replica_count_reaches_the_automatic_ceiling() {
        let load = Ok(crate::engine_metrics::EngineLoad::default());
        assert_eq!(auto_settings_from_probe(&load, 4).unwrap().max_concurrent, 4);
        assert_eq!(auto_settings_from_probe(&load, 1).unwrap().max_concurrent, 16);
    }

    /// An endpoint that answers "not an engine" is a verdict and restores the
    /// gate at once; a probe that got no answer might be the overload itself
    /// and waits for the last reading to go stale.
    #[test]
    fn only_an_unanswered_probe_waits_for_staleness_before_restoring() {
        assert!(!restore_only_if_silent(&ProbeError::NotAnEngine("404".into())));
        assert!(restore_only_if_silent(&ProbeError::Unreachable("timeout".into())));
    }
}
