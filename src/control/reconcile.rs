//! Control-plane side of P2 reconciliation: aggregates each replica's
//! self-reported request/token counts per principal and answers with that
//! replica's fair share of the principal's *configured* limit for the next
//! window.
//!
//! Deliberately in-memory, not persisted in Postgres: this is operational
//! demand data that is meant to go stale within a couple of reporting
//! intervals (see `STALE_AFTER`), not a record anyone needs after a control
//! plane restart. A restart simply means every replica starts back at "full
//! share" until its next report lands -- the same "may exceed by up to one
//! window" cost the design doc already accepts for a reconciliation gap, not
//! a new one.
//!
//! The aggregation itself (`ReconcileState::report`) is a pure function of
//! its inputs and is exercised directly in this module's tests, without a
//! database or an HTTP round trip -- `crate::control::api::post_reconcile`
//! is a thin wire-format wrapper around it.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A replica's most-recently-reported counts for one principal. Reports
/// replace, they do not accumulate -- `crate::limiter::Limiter::drain_counts`
/// already zeroes its counters on every flush, so the value handed to
/// `report` is always "since last time", and keeping only the latest one per
/// replica is what "most recently observed" means here.
#[derive(Debug, Clone, Copy)]
struct ReplicaCount {
    requests: u64,
    tokens: u64,
    seen_at: Instant,
}

/// Reports older than this are excluded from the aggregate: a replica that
/// stopped reporting (scaled down, crashed, network partition) must not keep
/// claiming a share of a principal's limit forever, starving every replica
/// still alive. Several multiples of the expected report interval (a few
/// seconds, per the design) so one merely-slow tick does not zero out a live
/// replica's share.
const STALE_AFTER: Duration = Duration::from_secs(30);

/// This replica's share of one principal's configured limit for the next
/// window, in each dimension. `0.0..=1.0`; `crate::limiter::Limiter`
/// multiplies this against the configured `requests_per_min`/`tokens_per_min`
/// to get an effective local bucket capacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Share {
    pub requests: f64,
    pub tokens: f64,
}

/// Per-principal, per-replica demand, and the aggregation that turns it into
/// a share. One instance lives on the control plane's `Ctx` for the life of
/// the process.
#[derive(Default)]
pub struct ReconcileState {
    by_principal: RwLock<HashMap<crate::snapshot::PrincipalId, HashMap<String, ReplicaCount>>>,
}

impl ReconcileState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `replica_id`'s counts for `principal_id`, prune stale entries,
    /// and answer with `replica_id`'s share of the total observed across
    /// every still-live replica.
    ///
    /// A principal nobody has reported traffic for yet (`total == 0`) gets
    /// the full share: there is nothing to divide, and answering `0.0`
    /// instead would round every principal's first few seconds of traffic
    /// down to no allowance at all, on every replica, until a second report
    /// changes the total away from zero.
    pub fn report(
        &self,
        replica_id: &str,
        principal_id: crate::snapshot::PrincipalId,
        requests: u64,
        tokens: u64,
        now: Instant,
    ) -> Share {
        let mut map = self.by_principal.write();
        let per_replica = map.entry(principal_id).or_default();
        per_replica.insert(
            replica_id.to_string(),
            ReplicaCount {
                requests,
                tokens,
                seen_at: now,
            },
        );
        per_replica.retain(|_, c| now.saturating_duration_since(c.seen_at) < STALE_AFTER);

        let total_requests: u64 = per_replica.values().map(|c| c.requests).sum();
        let total_tokens: u64 = per_replica.values().map(|c| c.tokens).sum();
        let mine = per_replica.get(replica_id).copied();
        let live_replicas = per_replica.len();

        Share {
            requests: share_of(
                mine.map(|c| c.requests).unwrap_or(0),
                total_requests,
                live_replicas,
            ),
            tokens: share_of(
                mine.map(|c| c.tokens).unwrap_or(0),
                total_tokens,
                live_replicas,
            ),
        }
    }
}

/// `live_replicas` is every replica this principal has a fresh report from
/// (including `mine` itself, and including replicas that reported zero) --
/// i.e. `per_replica.len()` at the point this is called.
fn share_of(mine: u64, total: u64, live_replicas: usize) -> f64 {
    if total == 0 {
        // Nobody has reported any traffic for this principal at all: there
        // is nothing to divide, and answering 0.0 would round every
        // replica's first few seconds down to no allowance on all of them
        // at once, until a second report changes the total away from zero.
        return 1.0;
    }
    if mine == 0 {
        // `mine == 0` with `total > 0` means *other* replicas have seen
        // this principal's traffic but this one has not -- an idle
        // replica, not a starved one. The naive `0 / total = 0.0` answer is
        // the real-production bug this floor exists to close:
        // `Limiter::check` multiplies the share straight into
        // `effective_capacity` (limiter.rs), and a capacity of exactly zero
        // makes `BucketState::take` reject *every* request for the rest of
        // the window no matter how far under its limit the principal
        // actually is on this replica -- total denial while under limit,
        // not the bounded over-admission this module's doc comment already
        // accepts as the design's cost.
        //
        // Floor at `1/live_replicas`: the natural "everyone gets an equal
        // slice" allowance for a replica with no demand history to divide
        // by. It costs at most one window of modest spare capacity sitting
        // idle on this replica (self-correcting at the next report, and
        // strictly milder than total denial); it comes out of nobody else's
        // share, because it is not subtracted from `total` -- a busy
        // replica's own `mine / total` is computed independently below and
        // is never reduced to pay for an idle peer's floor. That means the
        // sum of every replica's share can exceed 1.0 while some replica is
        // idle, but only ever by that idle replica's own floor, and it
        // collapses back to summing to exactly 1.0 the moment every replica
        // has nonzero demand again (the normal, steady-state case) -- which
        // is exactly when staying tight to the configured limit matters
        // most.
        return (1.0 / live_replicas.max(1) as f64).min(1.0);
    }
    (mine as f64 / total as f64).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lone_replica_gets_the_full_share() {
        let state = ReconcileState::new();
        let now = Instant::now();
        let share = state.report("replica-a", crate::snapshot::tid(1), 100, 5000, now);
        assert_eq!(
            share,
            Share {
                requests: 1.0,
                tokens: 1.0
            }
        );
    }

    #[test]
    fn two_equal_replicas_split_evenly() {
        let state = ReconcileState::new();
        let now = Instant::now();
        let a = state.report("replica-a", crate::snapshot::tid(1), 50, 500, now);
        let b = state.report("replica-b", crate::snapshot::tid(1), 50, 500, now);
        assert_eq!(
            a,
            Share {
                requests: 1.0,
                tokens: 1.0
            },
            "a was the only report at the time it asked"
        );
        assert_eq!(
            b,
            Share {
                requests: 0.5,
                tokens: 0.5
            }
        );
        // Ask again for `a` now that `b` has also reported: the split is
        // now visible to both.
        let a_again = state.report("replica-a", crate::snapshot::tid(1), 50, 500, now);
        assert_eq!(
            a_again,
            Share {
                requests: 0.5,
                tokens: 0.5
            }
        );
    }

    /// The property the design doc names explicitly: a replica handling more
    /// of a principal's traffic must come away with a larger allowance, not
    /// an equal or smaller one.
    #[test]
    fn a_replica_taking_more_traffic_gets_a_larger_share() {
        let state = ReconcileState::new();
        let now = Instant::now();
        state.report("busy", crate::snapshot::tid(1), 900, 9000, now);
        let quiet = state.report("quiet", crate::snapshot::tid(1), 100, 1000, now);
        let busy_again = state.report("busy", crate::snapshot::tid(1), 900, 9000, now);

        assert_eq!(
            quiet,
            Share {
                requests: 0.1,
                tokens: 0.1
            }
        );
        assert_eq!(
            busy_again,
            Share {
                requests: 0.9,
                tokens: 0.9
            }
        );
        assert!(busy_again.requests > quiet.requests);
        assert!(busy_again.tokens > quiet.tokens);
    }

    #[test]
    fn the_two_dimensions_split_independently() {
        // `busy` sends almost all the requests but almost none of the
        // tokens (short prompts, tiny completions); `quiet` is the reverse.
        // The two shares must not be conflated into one number.
        let state = ReconcileState::new();
        let now = Instant::now();
        state.report("busy", crate::snapshot::tid(1), 90, 10, now);
        let quiet = state.report("quiet", crate::snapshot::tid(1), 10, 90, now);
        assert_eq!(
            quiet,
            Share {
                requests: 0.1,
                tokens: 0.9
            }
        );
    }

    #[test]
    fn a_stale_replica_is_excluded_and_stops_diluting_the_share() {
        let state = ReconcileState::new();
        let t0 = Instant::now();
        state.report("gone", crate::snapshot::tid(1), 500, 500, t0);
        let mine_while_fresh = state.report("me", crate::snapshot::tid(1), 500, 500, t0);
        assert_eq!(
            mine_while_fresh,
            Share {
                requests: 0.5,
                tokens: 0.5
            }
        );

        let later = t0 + STALE_AFTER + Duration::from_secs(1);
        let mine_after_gone_expired = state.report("me", crate::snapshot::tid(1), 500, 500, later);
        assert_eq!(
            mine_after_gone_expired,
            Share {
                requests: 1.0,
                tokens: 1.0
            },
            "a replica that stopped reporting must not keep claiming a share forever"
        );
    }

    #[test]
    fn a_principal_with_no_reported_traffic_gets_the_full_share() {
        let state = ReconcileState::new();
        let now = Instant::now();
        let share = state.report("only-replica", crate::snapshot::tid(42), 0, 0, now);
        assert_eq!(
            share,
            Share {
                requests: 1.0,
                tokens: 1.0
            }
        );
    }

    #[test]
    fn principals_are_aggregated_independently() {
        let state = ReconcileState::new();
        let now = Instant::now();
        state.report("a", crate::snapshot::tid(1), 100, 100, now);
        state.report("a", crate::snapshot::tid(2), 900, 900, now);
        let share_p1 = state.report("b", crate::snapshot::tid(1), 100, 100, now);
        let share_p2 = state.report("b", crate::snapshot::tid(2), 100, 100, now);
        assert_eq!(
            share_p1,
            Share {
                requests: 0.5,
                tokens: 0.5
            }
        );
        assert_eq!(
            share_p2,
            Share {
                requests: 0.1,
                tokens: 0.1
            }
        );
    }

    /// Regression test for the real-production bug: a replica that saw zero
    /// traffic for a principal in the last window must not be handed a
    /// literal-zero share, or `Limiter::check` denies every request for that
    /// principal on this replica no matter how far under its limit it is.
    #[test]
    fn an_idle_replica_does_not_collapse_to_a_zero_share() {
        let state = ReconcileState::new();
        let now = Instant::now();
        state.report("busy", crate::snapshot::tid(1), 100, 100, now);
        let idle = state.report("idle", crate::snapshot::tid(1), 0, 0, now);
        assert!(
            idle.requests > 0.0,
            "an idle replica must still get some allowance"
        );
        assert!(
            idle.tokens > 0.0,
            "an idle replica must still get some allowance"
        );
        // Two live replicas -> the floor is 1/2.
        assert_eq!(idle.requests, 0.5);
        assert_eq!(idle.tokens, 0.5);
    }

    /// The floor an idle replica gets must not be paid for by shrinking a
    /// genuinely busy replica's own share -- otherwise fixing the idle case
    /// would create a new, milder version of the same starvation bug for the
    /// replica that is actually handling the traffic.
    #[test]
    fn an_idle_replicas_floor_does_not_shrink_a_busy_peers_share() {
        let state = ReconcileState::new();
        let now = Instant::now();
        let busy = state.report("busy", crate::snapshot::tid(1), 100, 100, now);
        assert_eq!(
            busy,
            Share {
                requests: 1.0,
                tokens: 1.0
            },
            "busy was the only report at the time it asked"
        );
        state.report("idle", crate::snapshot::tid(1), 0, 0, now);
        let busy_again = state.report("busy", crate::snapshot::tid(1), 100, 100, now);
        assert_eq!(
            busy_again,
            Share {
                requests: 1.0,
                tokens: 1.0
            },
            "an idle peer's floor must not come out of the busy replica's own share"
        );
    }

    /// In the normal case -- every replica has some genuine, unequal demand,
    /// nobody idle -- the floor never applies, and shares must add up to
    /// exactly the configured limit's worth, never more. This is the
    /// property that would break if the idle-replica floor above were
    /// applied unconditionally instead of only when `mine == 0`.
    #[test]
    fn steady_state_shares_never_sum_above_one() {
        let state = ReconcileState::new();
        let now = Instant::now();
        state.report("a", crate::snapshot::tid(1), 500, 500, now);
        state.report("b", crate::snapshot::tid(1), 300, 300, now);
        state.report("c", crate::snapshot::tid(1), 200, 200, now);
        // Re-report each now that all three are visible, to read back the
        // final, fully-aggregated share for every one of them.
        let a = state.report("a", crate::snapshot::tid(1), 500, 500, now);
        let b = state.report("b", crate::snapshot::tid(1), 300, 300, now);
        let c = state.report("c", crate::snapshot::tid(1), 200, 200, now);

        let total_requests = a.requests + b.requests + c.requests;
        let total_tokens = a.tokens + b.tokens + c.tokens;
        assert!(
            (total_requests - 1.0).abs() < 1e-9,
            "expected shares to sum to exactly 1.0, got {total_requests}"
        );
        assert!(
            (total_tokens - 1.0).abs() < 1e-9,
            "expected shares to sum to exactly 1.0, got {total_tokens}"
        );
    }
}
