//! P2 rate limiting (design doc, "P2 -- Rate limiting"): a local token
//! bucket per principal per replica, enforced with one hash lookup and a
//! short, synchronous critical section -- no I/O, no allocation once a
//! principal's bucket exists, and never a lock held across an `.await`.
//!
//! Accuracy across replicas comes from periodic reconciliation
//! (`crate::reconcile`), not from a shared counter: every few seconds each
//! proxy reports what it has observed locally, and the control plane hands
//! back this replica's *share* of the principal's configured limit for the
//! next window. Until the first reconciliation lands -- at startup, or after
//! a control-plane outage -- every replica enforces the *full* configured
//! limit locally, which is deliberately generous rather than deliberately
//! strict: the design's accepted cost is that a limit can be exceeded by up
//! to one reconciliation window's worth of traffic during a sharp spike, not
//! that a cold-started replica starves a principal it knows nothing about
//! yet.
//!
//! `--role all` never spawns the reconciliation client at all (see
//! `main.rs`), so every principal's share simply stays at its default of
//! "full" forever -- one process's local counters already are the global
//! counters, so there is nothing to reconcile and nothing here needs to know
//! that it is running in that mode.

use crate::snapshot::PrincipalId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A principal's configured limits, pre-resolved into the snapshot exactly
/// like `Principal::allowed_models` -- see that field's doc comment for why
/// resolving once at snapshot-build time rather than per request is the
/// whole point of this architecture.
///
/// The two dimensions are independent: either may be configured alone. Both
/// `None` (`Limits::default()`) means unlimited, and critically that is
/// represented as the *absence* of a bucket, not a bucket with capacity
/// zero -- getting that backwards would deny every request from every
/// principal with no configured limit, which is most of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    pub requests_per_min: Option<u32>,
    pub tokens_per_min: Option<u32>,
}

impl Limits {
    pub fn is_unlimited(&self) -> bool {
        self.requests_per_min.is_none() && self.tokens_per_min.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Decision {
    Admitted,
    /// How long until at least one more unit would be available, for the
    /// `Retry-After` header. Rounded up by the caller -- fractional seconds
    /// are not a meaningful unit for that header.
    Exceeded {
        retry_after: Duration,
    },
}

/// A single dimension's bucket. `available` and `last_refill` are protected
/// together by one `Mutex` (see `PrincipalState`) rather than split across
/// independent atomics: the refill-then-take sequence has to see a
/// consistent pair or a burst of concurrent requests could each observe a
/// stale `available` and jointly overdraw the bucket. The critical section
/// is a handful of float operations, never awaited across, so this does not
/// reintroduce the I/O-on-the-hot-path problem the design exists to avoid --
/// it is exactly the kind of short, synchronous lock `Registry`'s own
/// in-flight counters already use elsewhere in this codebase.
struct BucketState {
    available: f64,
    last_refill: Instant,
}

impl BucketState {
    fn new(capacity: f64, now: Instant) -> Self {
        Self {
            available: capacity,
            last_refill: now,
        }
    }

    /// Refill for elapsed time (capped at `capacity`), then attempt to take
    /// `amount`. `capacity` is passed in per call rather than stored,
    /// because it can change between calls -- a reconciliation response
    /// narrowing this replica's share, or an admin edit to the configured
    /// limit reaching this principal on the next snapshot -- and a bucket
    /// whose capacity shrinks must not keep however much `available` it
    /// already had; `.min(capacity)` on every refill is what clamps it down
    /// on the very next call instead of only whenever the bucket happens to
    /// empty out on its own.
    fn take(&mut self, amount: f64, capacity: f64, now: Instant) -> Result<(), Duration> {
        let rate_per_sec = capacity / 60.0;
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.available = (self.available + elapsed * rate_per_sec).min(capacity);
        self.last_refill = now;

        if self.available >= amount {
            self.available -= amount;
            return Ok(());
        }
        if rate_per_sec <= 0.0 {
            // Capacity collapsed to zero (a reconciled share of 0, or a
            // configured limit of 0 slipping past admin validation): no
            // amount of waiting refills it, so there is no honest finite
            // `Retry-After` to give. One window is a safe, bounded answer --
            // this state is not expected to persist, since reconciliation
            // runs again well within a minute.
            return Err(Duration::from_secs(60));
        }
        let deficit = amount - self.available;
        Err(Duration::from_secs_f64((deficit / rate_per_sec).max(0.0)))
    }
}

/// Fixed-point share, in millionths of "this replica's full configured
/// limit". Stored in an `AtomicU32` (rather than the `f64` the wire protocol
/// uses) so applying a reconciliation result never needs the bucket's own
/// lock -- share and bucket are read together inside `check` but written
/// independently, by different tasks, on different schedules.
const SHARE_SCALE: u32 = 1_000_000;

struct PrincipalState {
    requests: Mutex<BucketState>,
    tokens: Mutex<BucketState>,
    requests_share: AtomicU32,
    tokens_share: AtomicU32,
    /// Observed demand since the last `drain_counts` flush -- incremented on
    /// every `check` call for a configured dimension, admitted or not, so a
    /// replica that is mostly *rejecting* a principal's traffic still
    /// reports accurate demand for `control::reconcile` to divide up, rather
    /// than looking idle because none of it got through.
    requests_used: AtomicU64,
    tokens_used: AtomicU64,
}

impl PrincipalState {
    fn new(limits: &Limits, now: Instant) -> Self {
        Self {
            requests: Mutex::new(BucketState::new(
                limits.requests_per_min.unwrap_or(0) as f64,
                now,
            )),
            tokens: Mutex::new(BucketState::new(
                limits.tokens_per_min.unwrap_or(0) as f64,
                now,
            )),
            requests_share: AtomicU32::new(SHARE_SCALE),
            tokens_share: AtomicU32::new(SHARE_SCALE),
            requests_used: AtomicU64::new(0),
            tokens_used: AtomicU64::new(0),
        }
    }
}

fn effective_capacity(configured: u32, share_millionths: u32) -> f64 {
    (configured as f64) * (share_millionths as f64 / SHARE_SCALE as f64)
}

fn share_to_millionths(share: f64) -> u32 {
    (share.clamp(0.0, 1.0) * SHARE_SCALE as f64).round() as u32
}

/// One principal's locally observed demand since the last flush -- the unit
/// `crate::reconcile` reports upward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedCounts {
    pub principal_id: PrincipalId,
    pub requests: u64,
    pub tokens: u64,
}

/// This replica's share of one principal's configured limit for the next
/// window, as returned by the control plane's `/limits/reconcile`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Allowance {
    pub principal_id: PrincipalId,
    /// 0.0..=1.0. Values outside that range are clamped, not rejected --
    /// see `share_to_millionths`.
    pub requests_share: f64,
    pub tokens_share: f64,
}

/// Per-replica, per-principal token buckets.
///
/// Deliberately independent of `Snapshot`: a principal's bucket must survive
/// a snapshot reload (an unrelated model or key edit that happens to trigger
/// a rebuild must not hand every principal a freshly-full bucket, or a
/// caller could launder a 429 into a fresh allowance by tripping any admin
/// write at all) and a reconciliation cycle (see `PrincipalState`'s share
/// fields, updated independently of whatever bucket state already exists).
/// `AppState` owns one `Limiter` for the life of the process; only the
/// `Limits` passed into `check` on each call comes from the current
/// snapshot.
pub struct Limiter {
    entries: parking_lot::RwLock<HashMap<PrincipalId, Arc<PrincipalState>>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

impl Limiter {
    pub fn new() -> Self {
        Self {
            entries: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    /// One hash lookup (a read lock over a `HashMap`, or -- the first time a
    /// principal is seen -- a write lock to insert one entry) plus, per
    /// configured dimension, a short lock-protected refill-and-decrement.
    /// No I/O, no heap allocation once the principal's entry exists.
    ///
    /// `token_cost` is an estimate, not a measurement: the proxy does not
    /// know how many tokens a request will actually consume until the
    /// response completes (see the design doc's P3 section on why usage
    /// accounting is inherently after-the-fact), so this is charged against
    /// the requested prompt size plus `max_tokens` -- both already available
    /// on the request path for P1's shape-matching routing rules, at no
    /// extra cost to compute here.
    pub fn check(
        &self,
        principal_id: PrincipalId,
        limits: &Limits,
        token_cost: u32,
        now: Instant,
    ) -> Decision {
        if limits.is_unlimited() {
            return Decision::Admitted;
        }
        let state = self.state_for(principal_id, limits, now);

        if let Some(configured) = limits.requests_per_min {
            state.requests_used.fetch_add(1, Ordering::Relaxed);
            let capacity =
                effective_capacity(configured, state.requests_share.load(Ordering::Relaxed));
            let mut bucket = state.requests.lock();
            if let Err(retry_after) = bucket.take(1.0, capacity, now) {
                return Decision::Exceeded { retry_after };
            }
        }
        if let Some(configured) = limits.tokens_per_min {
            let cost = token_cost.max(1) as u64;
            state.tokens_used.fetch_add(cost, Ordering::Relaxed);
            let capacity =
                effective_capacity(configured, state.tokens_share.load(Ordering::Relaxed));
            let mut bucket = state.tokens.lock();
            if let Err(retry_after) = bucket.take(cost as f64, capacity, now) {
                return Decision::Exceeded { retry_after };
            }
        }
        Decision::Admitted
    }

    fn state_for(
        &self,
        principal_id: PrincipalId,
        limits: &Limits,
        now: Instant,
    ) -> Arc<PrincipalState> {
        if let Some(existing) = self.entries.read().get(&principal_id) {
            return Arc::clone(existing);
        }
        let mut entries = self.entries.write();
        Arc::clone(
            entries
                .entry(principal_id)
                .or_insert_with(|| Arc::new(PrincipalState::new(limits, now))),
        )
    }

    /// Snapshot every principal's observed-demand counters and zero them.
    /// Called by `crate::reconcile` on its own schedule, never from the
    /// request path.
    pub fn drain_counts(&self) -> Vec<ObservedCounts> {
        self.entries
            .read()
            .iter()
            .map(|(id, state)| ObservedCounts {
                principal_id: *id,
                requests: state.requests_used.swap(0, Ordering::Relaxed),
                tokens: state.tokens_used.swap(0, Ordering::Relaxed),
            })
            .collect()
    }

    /// Apply the control plane's answer. A principal not mentioned in
    /// `allowances` keeps whatever share it already had -- `SHARE_SCALE`
    /// (full) until the first reconciliation ever runs for it, which is the
    /// generous default this module's doc comment describes.
    pub fn apply_allowances(&self, allowances: &[Allowance]) {
        let entries = self.entries.read();
        for a in allowances {
            if let Some(state) = entries.get(&a.principal_id) {
                state
                    .requests_share
                    .store(share_to_millionths(a.requests_share), Ordering::Relaxed);
                state
                    .tokens_share
                    .store(share_to_millionths(a.tokens_share), Ordering::Relaxed);
            }
        }
    }

    #[cfg(test)]
    fn share_of(&self, principal_id: PrincipalId) -> Option<(f64, f64)> {
        self.entries.read().get(&principal_id).map(|s| {
            (
                s.requests_share.load(Ordering::Relaxed) as f64 / SHARE_SCALE as f64,
                s.tokens_share.load(Ordering::Relaxed) as f64 / SHARE_SCALE as f64,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(requests: Option<u32>, tokens: Option<u32>) -> Limits {
        Limits {
            requests_per_min: requests,
            tokens_per_min: tokens,
        }
    }

    #[test]
    fn a_principal_with_no_configured_limit_is_unlimited_not_zero() {
        let limiter = Limiter::new();
        let now = Instant::now();
        for _ in 0..1000 {
            assert_eq!(
                limiter.check(1, &Limits::default(), 1, now),
                Decision::Admitted,
                "an unconfigured principal must never be denied"
            );
        }
        // No bucket was ever allocated for it: unlimited must not cost a map
        // entry every process keeps forever.
        assert!(limiter.entries.read().get(&1).is_none());
    }

    #[test]
    fn a_bucket_admits_exactly_its_allowance_and_then_429s() {
        let limiter = Limiter::new();
        let lim = limits(Some(3), None);
        let now = Instant::now();
        for i in 0..3 {
            assert_eq!(
                limiter.check(1, &lim, 1, now),
                Decision::Admitted,
                "request {i} of 3 should be admitted"
            );
        }
        match limiter.check(1, &lim, 1, now) {
            Decision::Exceeded { retry_after } => assert!(retry_after > Duration::ZERO),
            Decision::Admitted => panic!("the 4th request must be rejected"),
        }
    }

    #[test]
    fn retry_after_is_sane() {
        let limiter = Limiter::new();
        // 60 requests/min == 1/sec, so exhausting the bucket and asking
        // again immediately should need to wait roughly one second -- not
        // zero, not an hour.
        let lim = limits(Some(60), None);
        let now = Instant::now();
        for _ in 0..60 {
            assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        }
        match limiter.check(1, &lim, 1, now) {
            Decision::Exceeded { retry_after } => {
                assert!(
                    retry_after >= Duration::from_millis(900)
                        && retry_after <= Duration::from_secs(2),
                    "expected roughly 1s, got {retry_after:?}"
                );
            }
            Decision::Admitted => panic!("bucket must be empty"),
        }
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let limiter = Limiter::new();
        let lim = limits(Some(60), None); // 1 token/sec
        let now = Instant::now();
        for _ in 0..60 {
            assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        }
        assert!(matches!(
            limiter.check(1, &lim, 1, now),
            Decision::Exceeded { .. }
        ));

        // Simulate elapsed time by advancing the clock passed in, rather
        // than sleeping -- deterministic and instant.
        let later = now + Duration::from_secs(5);
        assert_eq!(
            limiter.check(1, &lim, 1, later),
            Decision::Admitted,
            "5 seconds at 1/sec should have refilled at least one slot"
        );
    }

    #[test]
    fn the_two_dimensions_are_enforced_independently() {
        let limiter = Limiter::new();
        // Plenty of requests, very few tokens.
        let lim = limits(Some(1000), Some(5));
        let now = Instant::now();

        assert_eq!(
            limiter.check(1, &lim, 5, now),
            Decision::Admitted,
            "exactly the token budget in one request"
        );
        assert!(
            matches!(limiter.check(1, &lim, 1, now), Decision::Exceeded { .. }),
            "tokens must be exhausted even though requests/min has huge headroom left"
        );

        // The mirror case: requests/min is the tight dimension.
        let limiter2 = Limiter::new();
        let lim2 = limits(Some(1), Some(1_000_000));
        assert_eq!(limiter2.check(2, &lim2, 1, now), Decision::Admitted);
        assert!(
            matches!(limiter2.check(2, &lim2, 1, now), Decision::Exceeded { .. }),
            "requests/min must reject even though tokens/min has huge headroom left"
        );
    }

    #[test]
    fn a_snapshot_reload_does_not_reset_an_in_progress_bucket() {
        // `Limiter` lives outside `Snapshot` on purpose -- see the module
        // doc comment. This pins that: calling `check` again with a fresh
        // `Limits` value (as if a new snapshot had just been applied) must
        // not hand the principal a newly-full bucket.
        let limiter = Limiter::new();
        let lim = limits(Some(2), None);
        let now = Instant::now();
        assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        // Same limit value, but a distinct `Limits` instance -- simulating a
        // snapshot rebuild that reproduced the identical configured limit.
        let lim_after_reload = limits(Some(2), None);
        assert!(matches!(
            limiter.check(1, &lim_after_reload, 1, now),
            Decision::Exceeded { .. }
        ));
    }

    #[test]
    fn reconciliation_narrows_the_local_share_and_therefore_the_effective_capacity() {
        let limiter = Limiter::new();
        let lim = limits(Some(100), None);
        let now = Instant::now();
        // Touch the principal once so a bucket exists to reconcile.
        assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        assert_eq!(limiter.share_of(1), Some((1.0, 1.0)));

        limiter.apply_allowances(&[Allowance {
            principal_id: 1,
            requests_share: 0.25,
            tokens_share: 1.0,
        }]);
        assert_eq!(limiter.share_of(1), Some((0.25, 1.0)));

        // The reduced capacity (100 * 0.25 = 25) is only applied by the next
        // `take` -- the one call above happened before `apply_allowances`
        // and is not retroactively subtracted from it -- so 25 more calls
        // are admitted before the bucket is actually empty at the new,
        // narrower capacity.
        for _ in 0..25 {
            assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        }
        assert!(matches!(
            limiter.check(1, &lim, 1, now),
            Decision::Exceeded { .. }
        ));
    }

    #[test]
    fn drain_counts_reports_and_zeroes_observed_demand() {
        let limiter = Limiter::new();
        let lim = limits(Some(1000), Some(1000));
        let now = Instant::now();
        limiter.check(1, &lim, 7, now);
        limiter.check(1, &lim, 3, now);

        let drained = limiter.drain_counts();
        let mine = drained.iter().find(|c| c.principal_id == 1).unwrap();
        assert_eq!(mine.requests, 2);
        assert_eq!(mine.tokens, 10);

        // A second drain with no traffic in between reports zero, not the
        // same counts again.
        let drained_again = limiter.drain_counts();
        let mine_again = drained_again.iter().find(|c| c.principal_id == 1).unwrap();
        assert_eq!(mine_again.requests, 0);
        assert_eq!(mine_again.tokens, 0);
    }

    #[test]
    fn a_rejected_request_still_counts_as_observed_demand() {
        // Otherwise a replica that is mostly rejecting a principal's traffic
        // would look idle to the reconciler and never earn a larger share
        // back even though real demand is arriving.
        let limiter = Limiter::new();
        let lim = limits(Some(1), None);
        let now = Instant::now();
        assert_eq!(limiter.check(1, &lim, 1, now), Decision::Admitted);
        assert!(matches!(
            limiter.check(1, &lim, 1, now),
            Decision::Exceeded { .. }
        ));
        let drained = limiter.drain_counts();
        let mine = drained.iter().find(|c| c.principal_id == 1).unwrap();
        assert_eq!(
            mine.requests, 2,
            "both the admitted and the rejected attempt count as demand"
        );
    }
}
