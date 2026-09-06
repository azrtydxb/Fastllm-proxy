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
    /// Admitted, with what to tell the client about its remaining allowance.
    ///
    /// `None` when this principal has no limits configured: publishing
    /// `x-ratelimit-remaining: 0` to an unlimited caller is worse than
    /// publishing nothing, because a well-behaved client would back off
    /// against a limit that does not exist.
    Admitted(Option<RateLimitStatus>),
    /// How long until at least one more unit would be available, for the
    /// `Retry-After` header. Rounded up by the caller -- fractional seconds
    /// are not a meaningful unit for that header.
    Exceeded { retry_after: Duration },
}

/// What a client needs to pace itself, in the `x-ratelimit-*` shape every
/// provider publishes.
///
/// Read from the buckets after the commit that admitted the request, while
/// their locks are already held — two float reads rather than a second pass
/// over the limiter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimitStatus {
    pub requests_limit: Option<u32>,
    pub requests_remaining: Option<u32>,
    pub tokens_limit: Option<u32>,
    pub tokens_remaining: Option<u32>,
    /// Until the tighter dimension has refilled completely.
    pub reset_after: Duration,
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

    /// Refill for elapsed time (capped at `capacity`) and report whether
    /// `amount` would be available, *without* committing the debit --
    /// `available`/`last_refill` are left untouched. `capacity` is passed
    /// in per call rather than stored, because it can change between calls
    /// -- a reconciliation response narrowing this replica's share, or an
    /// admin edit to the configured limit reaching this principal on the
    /// next snapshot -- and a bucket whose capacity shrinks must not keep
    /// however much `available` it already had; `.min(capacity)` here is
    /// what clamps it down on the very next call instead of only whenever
    /// the bucket happens to empty out on its own.
    ///
    /// Split from the actual debit (`commit`, below) so `Limiter::check`
    /// can evaluate both dimensions -- requests and tokens are independent
    /// `BucketState`s -- before committing either one. See `check`'s own
    /// comment for why that ordering matters: committing a dimension that
    /// turns out not to matter (because the *other* dimension is what ends
    /// up rejecting the request) would waste real capacity on a request the
    /// caller never got admitted for.
    fn peek(&self, amount: f64, capacity: f64, now: Instant) -> Result<(), Duration> {
        let available = self.refilled(capacity, now);
        if available >= amount {
            return Ok(());
        }
        if capacity <= 0.0 {
            // Capacity collapsed to zero (a reconciled share of 0, or a
            // configured limit of 0 slipping past admin validation): no
            // amount of waiting refills it, so there is no honest finite
            // `Retry-After` to give. One window is a safe, bounded answer --
            // this state is not expected to persist, since reconciliation
            // runs again well within a minute.
            return Err(Duration::from_secs(60));
        }
        let deficit = amount - available;
        Err(Duration::from_secs_f64(
            (deficit / (capacity / 60.0)).max(0.0),
        ))
    }

    /// Actually refill and debit `amount`. Only ever called after `peek`
    /// (with the same `capacity`/`now`) has already confirmed `amount` is
    /// available -- this does not re-check, it trusts the caller, which is
    /// exactly what lets `Limiter::check` decide both dimensions first and
    /// commit only the ones a fully-admitted request actually needs.
    fn commit(&mut self, amount: f64, capacity: f64, now: Instant) {
        self.available = self.refilled(capacity, now) - amount;
        self.last_refill = now;
    }

    /// The shared refill arithmetic `peek` and `commit` both need, kept in
    /// one place so the two can never drift out of agreement with each
    /// other about how much a given `elapsed`/`capacity` refills.
    fn refilled(&self, capacity: f64, now: Instant) -> f64 {
        let rate_per_sec = capacity / 60.0;
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        (self.available + elapsed * rate_per_sec).min(capacity)
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
    /// Wall-clock time of this principal's most recent `check` call,
    /// regardless of which dimension was configured or whether it admitted
    /// -- what `Limiter::evict_idle` reads to decide whether this entry's
    /// traffic has gone quiet long enough to reclaim. A `Mutex<Instant>`
    /// rather than an atomic: `Instant` has no lock-free representation on
    /// stable Rust, and this is touched at most once per `check` call, no
    /// more often than the two bucket locks already are.
    last_seen: Mutex<Instant>,
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
            last_seen: Mutex::new(now),
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
///
/// `entries` is one unsharded `RwLock<HashMap<..>>`, not sharded across
/// several locks/maps the way some high-throughput rate limiters split by
/// key hash. Deliberately: sharding buys concurrency on the *write* path
/// (inserting a never-before-seen principal, or `evict_idle`'s sweep), and
/// `check`'s overwhelmingly common case never takes that path at all -- it
/// is a read lock over an already-populated map, and `parking_lot::RwLock`
/// allows unlimited concurrent readers with no shared mutable state between
/// them. The number of distinct entries is bounded by the number of
/// *rate-limited* principals a real deployment configures, not by request
/// volume -- tens to low thousands, not millions -- so a single map's
/// lookup cost and its read-lock's uncontended-fast-path cost both stay
/// negligible at any scale this proxy actually runs at. Sharding would trade
/// that simplicity for a real cost (memory overhead per shard, and a global
/// operation like `evict_idle` or `drain_counts` having to walk N locks
/// instead of one) to solve a contention problem this workload does not
/// have. Revisit if `check`'s write path (new-principal inserts) ever shows
/// up as real contention under load, not before.
pub struct Limiter {
    entries: parking_lot::RwLock<HashMap<PrincipalId, Arc<PrincipalState>>>,
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new()
    }
}

/// How long a principal's bucket may sit untouched before `evict_idle`
/// reclaims it. Long enough that a principal with merely bursty (not
/// abandoned) traffic — an hourly cron job, a human's on-and-off usage
/// through a day — never gets evicted mid-pattern only to pay for a full
/// bucket recreation (see `evict_idle`'s doc comment on why that recreation
/// is safe, just not free of the "first reconciliation after a restart"
/// cost); short enough that a real deployment's actual churn (principals
/// deleted, keys rotated to a new principal, a load test's throwaway
/// principals) does not accumulate for the life of the process the way
/// nothing before this constant existed prevented.
pub const IDLE_EVICTION_AFTER: Duration = Duration::from_secs(30 * 60);

impl Limiter {
    pub fn new() -> Self {
        Self {
            entries: parking_lot::RwLock::new(HashMap::new()),
        }
    }

    /// Remove every principal's bucket untouched by `check` for at least
    /// `idle_after`. Nothing on the request path calls this — see
    /// `main.rs` for where it is scheduled on its own periodic tick,
    /// mirroring `crate::reconcile`'s "own I/O on its own schedule, never
    /// on the request path" shape even though this itself does no I/O.
    ///
    /// This is the fix for `entries` otherwise growing without bound as
    /// principals come and go: a deleted principal, a rotated key's old
    /// principal, a load test's throwaway keys all used to leave a
    /// `HashMap` entry behind forever, since nothing ever removed one.
    ///
    /// Evicting outright (not just "safe to be slow about", but safe
    /// *at all*) relies on `PrincipalState::new` always starting a fresh
    /// bucket at full configured capacity and full share (`SHARE_SCALE`) --
    /// the same generous default a cold-started replica already gets before
    /// its first reconciliation. A principal whose bucket is evicted and
    /// then sees traffic again simply looks cold-started again: capacity
    /// this module's own doc comment already accepts giving away, not a new
    /// failure mode this eviction introduces.
    pub fn evict_idle(&self, now: Instant, idle_after: Duration) {
        self.entries
            .write()
            .retain(|_, state| now.saturating_duration_since(*state.last_seen.lock()) < idle_after);
    }

    /// One hash lookup (a read lock over a `HashMap`, or -- the first time a
    /// principal is seen -- a write lock to insert one entry) plus, per
    /// configured dimension, a short lock-protected refill check and --
    /// only if the request is admitted overall -- a commit. No I/O, no heap
    /// allocation once the principal's entry exists.
    ///
    /// The two configured dimensions are decided before either is
    /// committed, and both locks (where held) stay held across that whole
    /// decide-then-commit sequence -- deliberately, for two reasons:
    ///
    /// - **Consistency with `requests_used`/`tokens_used`** (see
    ///   `PrincipalState`'s doc comment on those fields): both are bumped
    ///   for every configured dimension regardless of which one -- if
    ///   either -- ends up rejecting, so a replica that is mostly rejecting
    ///   a principal's tokens-heavy traffic still reports accurate token
    ///   demand even on the requests it never got far enough to look at
    ///   tokens for under the old sequential-short-circuit shape.
    /// - **No wasted capacity.** Deciding both dimensions before debiting
    ///   either means a request that is going to be 429'd by *tokens* never
    ///   consumes a slot from the *requests* bucket first -- the old shape
    ///   committed `requests` immediately upon success, only to discover
    ///   the overall answer was `Exceeded` once it got to `tokens`, wasting
    ///   one real admission the caller never benefited from.
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
            return Decision::Admitted(None);
        }
        let state = self.state_for(principal_id, limits, now);
        *state.last_seen.lock() = now;
        let token_cost = token_cost.max(1) as f64;

        // Demand is counted for every configured dimension unconditionally,
        // before either bucket is even peeked -- see the doc comment above.
        if limits.requests_per_min.is_some() {
            state.requests_used.fetch_add(1, Ordering::Relaxed);
        }
        if limits.tokens_per_min.is_some() {
            state
                .tokens_used
                .fetch_add(token_cost as u64, Ordering::Relaxed);
        }

        // Both locks (where the dimension is configured) are acquired here
        // and held through both the peek below and the commit further
        // down, so no other `check` call for this principal can interleave
        // a commit of its own in between this call's decision and its own.
        let mut requests_bucket = limits.requests_per_min.map(|configured| {
            let capacity =
                effective_capacity(configured, state.requests_share.load(Ordering::Relaxed));
            (state.requests.lock(), capacity)
        });
        let mut tokens_bucket = limits.tokens_per_min.map(|configured| {
            let capacity =
                effective_capacity(configured, state.tokens_share.load(Ordering::Relaxed));
            (state.tokens.lock(), capacity)
        });

        let requests_peek = requests_bucket
            .as_ref()
            .map(|(bucket, capacity)| bucket.peek(1.0, *capacity, now));
        let tokens_peek = tokens_bucket
            .as_ref()
            .map(|(bucket, capacity)| bucket.peek(token_cost, *capacity, now));

        // Whichever configured dimension is tighter decides the wait: if
        // only one dimension rejects, its own `retry_after` is reported
        // unchanged from before this fix; if both reject, the caller
        // cannot usefully retry before the slower of the two clears
        // anyway, so the longer wait is the honest answer.
        let retry_after = [&requests_peek, &tokens_peek]
            .into_iter()
            .flatten()
            .filter_map(|r| r.err())
            .max();
        if let Some(retry_after) = retry_after {
            return Decision::Exceeded { retry_after };
        }

        if let Some((bucket, capacity)) = requests_bucket.as_mut() {
            bucket.commit(1.0, *capacity, now);
        }
        if let Some((bucket, capacity)) = tokens_bucket.as_mut() {
            bucket.commit(token_cost, *capacity, now);
        }

        // Read while the locks are still held, so what is reported is what this
        // request actually left behind rather than a later snapshot another
        // request may already have moved.
        //
        // Floored, not rounded: 0.6 of a request is not one a client can spend,
        // and rounding up to 1 invites a retry certain to be refused.
        let left = |b: &Option<(parking_lot::MutexGuard<'_, BucketState>, f64)>| {
            b.as_ref().map(|(g, _)| g.available.max(0.0).floor() as u32)
        };
        // A token bucket has no discrete window to reset, so "when is the
        // allowance back" is the honest reading of `x-ratelimit-reset`. A full
        // bucket reports 0.
        let refill = |b: &Option<(parking_lot::MutexGuard<'_, BucketState>, f64)>| {
            b.as_ref().map_or(0.0, |(g, capacity)| {
                let per_sec = capacity / 60.0;
                if per_sec <= 0.0 {
                    60.0
                } else {
                    ((capacity - g.available).max(0.0) / per_sec).min(60.0)
                }
            })
        };
        let status = RateLimitStatus {
            requests_limit: limits.requests_per_min,
            requests_remaining: left(&requests_bucket),
            tokens_limit: limits.tokens_per_min,
            tokens_remaining: left(&tokens_bucket),
            // The tighter dimension decides, for the same reason it decides
            // `retry_after` above: a client cannot usefully retry before the
            // slower of the two has recovered.
            reset_after: Duration::from_secs_f64(
                refill(&requests_bucket).max(refill(&tokens_bucket)).ceil(),
            ),
        };
        Decision::Admitted(Some(status))
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

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries.read().len()
    }
}

/// How often `spawn_eviction`'s background sweep runs. A fraction of
/// [`IDLE_EVICTION_AFTER`] rather than some fixed small number: an entry is
/// never reclaimed more than one sweep interval later than it strictly had
/// to be, and there is no reason to poll far more often than that bound
/// actually needs.
const EVICTION_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Spawn the background task that periodically calls [`Limiter::evict_idle`].
/// Structurally the same shape as `crate::health::spawn`: an owned `Arc`,
/// its own `tokio::time::interval`, no return value -- nothing on the
/// request path calls back into this or depends on it running at all
/// (a process that never called this would simply keep every principal's
/// bucket forever, the pre-fix behaviour, not a correctness bug).
pub fn spawn_eviction(limiter: Arc<Limiter>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(EVICTION_SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            limiter.evict_idle(Instant::now(), IDLE_EVICTION_AFTER);
        }
    });
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
            assert!(
                matches!(
                    limiter.check(crate::snapshot::tid(1), &Limits::default(), 1, now),
                    Decision::Admitted(_)
                ),
                "an unconfigured principal must never be denied"
            );
        }
        // No bucket was ever allocated for it: unlimited must not cost a map
        // entry every process keeps forever.
        assert!(limiter
            .entries
            .read()
            .get(&crate::snapshot::tid(1))
            .is_none());
    }

    #[test]
    fn a_bucket_admits_exactly_its_allowance_and_then_429s() {
        let limiter = Limiter::new();
        let lim = limits(Some(3), None);
        let now = Instant::now();
        for i in 0..3 {
            assert!(
                matches!(
                    limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                    Decision::Admitted(_)
                ),
                "request {i} of 3 should be admitted"
            );
        }
        match limiter.check(crate::snapshot::tid(1), &lim, 1, now) {
            Decision::Exceeded { retry_after } => assert!(retry_after > Duration::ZERO),
            Decision::Admitted(_) => panic!("the 4th request must be rejected"),
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
            assert!(matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                Decision::Admitted(_)
            ));
        }
        match limiter.check(crate::snapshot::tid(1), &lim, 1, now) {
            Decision::Exceeded { retry_after } => {
                assert!(
                    retry_after >= Duration::from_millis(900)
                        && retry_after <= Duration::from_secs(2),
                    "expected roughly 1s, got {retry_after:?}"
                );
            }
            Decision::Admitted(_) => panic!("bucket must be empty"),
        }
    }

    #[test]
    fn the_bucket_refills_over_time() {
        let limiter = Limiter::new();
        let lim = limits(Some(60), None); // 1 token/sec
        let now = Instant::now();
        for _ in 0..60 {
            assert!(matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                Decision::Admitted(_)
            ));
        }
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Exceeded { .. }
        ));

        // Simulate elapsed time by advancing the clock passed in, rather
        // than sleeping -- deterministic and instant.
        let later = now + Duration::from_secs(5);
        assert!(
            matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 1, later),
                Decision::Admitted(_)
            ),
            "5 seconds at 1/sec should have refilled at least one slot"
        );
    }

    #[test]
    fn the_two_dimensions_are_enforced_independently() {
        let limiter = Limiter::new();
        // Plenty of requests, very few tokens.
        let lim = limits(Some(1000), Some(5));
        let now = Instant::now();

        assert!(
            matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 5, now),
                Decision::Admitted(_)
            ),
            "exactly the token budget in one request"
        );
        assert!(
            matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                Decision::Exceeded { .. }
            ),
            "tokens must be exhausted even though requests/min has huge headroom left"
        );

        // The mirror case: requests/min is the tight dimension.
        let limiter2 = Limiter::new();
        let lim2 = limits(Some(1), Some(1_000_000));
        assert!(matches!(
            limiter2.check(crate::snapshot::tid(2), &lim2, 1, now),
            Decision::Admitted(_)
        ));
        assert!(
            matches!(
                limiter2.check(crate::snapshot::tid(2), &lim2, 1, now),
                Decision::Exceeded { .. }
            ),
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
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Admitted(_)
        ));
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Admitted(_)
        ));
        // Same limit value, but a distinct `Limits` instance -- simulating a
        // snapshot rebuild that reproduced the identical configured limit.
        let lim_after_reload = limits(Some(2), None);
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim_after_reload, 1, now),
            Decision::Exceeded { .. }
        ));
    }

    #[test]
    fn reconciliation_narrows_the_local_share_and_therefore_the_effective_capacity() {
        let limiter = Limiter::new();
        let lim = limits(Some(100), None);
        let now = Instant::now();
        // Touch the principal once so a bucket exists to reconcile.
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Admitted(_)
        ));
        assert_eq!(limiter.share_of(crate::snapshot::tid(1)), Some((1.0, 1.0)));

        limiter.apply_allowances(&[Allowance {
            principal_id: crate::snapshot::tid(1),
            requests_share: 0.25,
            tokens_share: 1.0,
        }]);
        assert_eq!(limiter.share_of(crate::snapshot::tid(1)), Some((0.25, 1.0)));

        // The reduced capacity (100 * 0.25 = 25) is only applied by the next
        // `take` -- the one call above happened before `apply_allowances`
        // and is not retroactively subtracted from it -- so 25 more calls
        // are admitted before the bucket is actually empty at the new,
        // narrower capacity.
        for _ in 0..25 {
            assert!(matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                Decision::Admitted(_)
            ));
        }
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Exceeded { .. }
        ));
    }

    #[test]
    fn drain_counts_reports_and_zeroes_observed_demand() {
        let limiter = Limiter::new();
        let lim = limits(Some(1000), Some(1000));
        let now = Instant::now();
        limiter.check(crate::snapshot::tid(1), &lim, 7, now);
        limiter.check(crate::snapshot::tid(1), &lim, 3, now);

        let drained = limiter.drain_counts();
        let mine = drained
            .iter()
            .find(|c| c.principal_id == crate::snapshot::tid(1))
            .unwrap();
        assert_eq!(mine.requests, 2);
        assert_eq!(mine.tokens, 10);

        // A second drain with no traffic in between reports zero, not the
        // same counts again.
        let drained_again = limiter.drain_counts();
        let mine_again = drained_again
            .iter()
            .find(|c| c.principal_id == crate::snapshot::tid(1))
            .unwrap();
        assert_eq!(mine_again.requests, 0);
        assert_eq!(mine_again.tokens, 0);
    }

    #[test]
    fn a_reconciled_share_that_is_never_literally_zero_still_admits_traffic() {
        // End-to-end pin for the idle-replica bug: `control::reconcile`'s
        // floor (see its `share_of`) guarantees this replica never receives
        // an `Allowance` of exactly 0.0 for a principal it hasn't seen
        // traffic from. This test is the other half -- confirming that once
        // such a nonzero-but-small share is applied here, `check` actually
        // admits requests with it, rather than `effective_capacity`
        // rounding it back down to nothing.
        let limiter = Limiter::new();
        let lim = limits(Some(100), None);
        let now = Instant::now();
        // Touch the principal once so a bucket exists to apply a share to.
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Admitted(_)
        ));

        // A small but nonzero floor share, as the idle-replica fix hands
        // out -- e.g. 1/4 live replicas.
        limiter.apply_allowances(&[Allowance {
            principal_id: crate::snapshot::tid(1),
            requests_share: 0.25,
            tokens_share: 0.25,
        }]);

        // effective_capacity = 100 * 0.25 = 25, so 25 requests should be
        // admitted, not zero.
        for i in 0..25 {
            assert!(
                matches!(
                    limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                    Decision::Admitted(_)
                ),
                "request {i} of 25 should be admitted under a nonzero floor share"
            );
        }
    }

    #[test]
    fn a_literal_zero_share_would_deny_every_request_demonstrating_why_the_floor_matters() {
        // Documents the bug this module's callers must never reintroduce:
        // if `control::reconcile` ever again handed back a literal 0.0
        // share for a principal with traffic elsewhere, `Limiter` itself
        // has no independent floor -- it trusts the share it's given. This
        // is why the floor lives in `control::reconcile::share_of`, not
        // here: by the time `Limiter::check` sees a share, it must already
        // be safe to multiply in.
        let limiter = Limiter::new();
        let lim = limits(Some(100), None);
        let now = Instant::now();
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Admitted(_)
        ));

        limiter.apply_allowances(&[Allowance {
            principal_id: crate::snapshot::tid(1),
            requests_share: 0.0,
            tokens_share: 0.0,
        }]);

        assert!(
            matches!(
                limiter.check(crate::snapshot::tid(1), &lim, 1, now),
                Decision::Exceeded { .. }
            ),
            "a literal-zero share denies every request, which is exactly why \
             control::reconcile::share_of must never produce one while total > 0"
        );
    }

    #[test]
    fn a_rejected_request_still_counts_as_observed_demand() {
        // Otherwise a replica that is mostly rejecting a principal's traffic
        // would look idle to the reconciler and never earn a larger share
        // back even though real demand is arriving.
        let limiter = Limiter::new();
        let lim = limits(Some(1), None);
        let now = Instant::now();
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Admitted(_)
        ));
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 1, now),
            Decision::Exceeded { .. }
        ));
        let drained = limiter.drain_counts();
        let mine = drained
            .iter()
            .find(|c| c.principal_id == crate::snapshot::tid(1))
            .unwrap();
        assert_eq!(
            mine.requests, 2,
            "both the admitted and the rejected attempt count as demand"
        );
    }

    #[test]
    fn an_exhausted_requests_bucket_still_counts_token_demand() {
        // Regression test: `requests_per_min` is exhausted (so the overall
        // decision is `Exceeded` from that dimension alone), but
        // `tokens_per_min` is also configured with plenty of headroom. The
        // old sequential shape returned as soon as `requests` rejected and
        // never even looked at `tokens`, so `tokens_used` stayed at 0 even
        // though this call carried real token demand -- undercounting
        // exactly the demand `control::reconcile` needs to divide up.
        let limiter = Limiter::new();
        let lim = limits(Some(1), Some(1_000_000));
        let now = Instant::now();
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 500, now),
            Decision::Admitted(_)
        ));
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 500, now),
            Decision::Exceeded { .. }
        ));
        let drained = limiter.drain_counts();
        let mine = drained
            .iter()
            .find(|c| c.principal_id == crate::snapshot::tid(1))
            .unwrap();
        assert_eq!(mine.requests, 2, "both attempts count as request demand");
        assert_eq!(
            mine.tokens, 1000,
            "token demand from the rejected call must still be counted, even \
             though `requests` was the dimension that rejected it"
        );
    }

    #[test]
    fn a_request_rejected_by_tokens_does_not_also_consume_a_requests_slot() {
        // Regression test: `requests_per_min` has room for many more calls,
        // but `tokens_per_min` is already exhausted. The old shape
        // committed (debited) the `requests` bucket as soon as it admitted,
        // before ever checking `tokens` -- wasting one real request slot on
        // a call that was going to be 429'd anyway. This pins that the
        // `requests` bucket's available capacity is unaffected by a call
        // that tokens alone rejects.
        let limiter = Limiter::new();
        let lim = limits(Some(1000), Some(10));
        let now = Instant::now();
        // One admitted call spends all 10 tokens and 1 of 1000 requests.
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 10, now),
            Decision::Admitted(_)
        ));
        // Tokens alone rejects this one -- requests still has 999 of 1000
        // left, vast headroom.
        assert!(matches!(
            limiter.check(crate::snapshot::tid(1), &lim, 10, now),
            Decision::Exceeded { .. }
        ));

        // Probe the requests bucket in isolation (tokens not configured
        // this time, so only the requests dimension can reject) to prove
        // its remaining capacity is 999, not 998: if the tokens-rejected
        // call above had wrongly debited a requests slot anyway, only 998
        // more would be admitted here before this dimension also 429s.
        let requests_only = limits(Some(1000), None);
        for i in 0..999 {
            assert!(
                matches!(
                    limiter.check(crate::snapshot::tid(1), &requests_only, 1, now),
                    Decision::Admitted(_)
                ),
                "request {i} of the remaining 999 should be admitted"
            );
        }
        assert!(
            matches!(
                limiter.check(crate::snapshot::tid(1), &requests_only, 1, now),
                Decision::Exceeded { .. }
            ),
            "the 1000th total request must now be rejected"
        );
    }

    #[test]
    fn evict_idle_reclaims_an_entry_untouched_since_before_the_cutoff() {
        let limiter = Limiter::new();
        let lim = limits(Some(10), None);
        let now = Instant::now();
        limiter.check(crate::snapshot::tid(1), &lim, 1, now);
        assert_eq!(limiter.entry_count(), 1);

        let much_later = now + Duration::from_secs(3600);
        limiter.evict_idle(much_later, Duration::from_secs(60));
        assert_eq!(
            limiter.entry_count(),
            0,
            "an entry with no traffic in the last hour must be reclaimed by a 60s idle cutoff"
        );
    }

    #[test]
    fn evict_idle_leaves_a_recently_touched_entry_alone() {
        let limiter = Limiter::new();
        let lim = limits(Some(10), None);
        let now = Instant::now();
        limiter.check(crate::snapshot::tid(1), &lim, 1, now);

        let soon_after = now + Duration::from_secs(5);
        limiter.evict_idle(soon_after, Duration::from_secs(60));
        assert_eq!(
            limiter.entry_count(),
            1,
            "an entry touched 5s ago must survive a 60s idle cutoff"
        );
    }

    #[test]
    fn evict_idle_does_not_reclaim_an_entry_that_keeps_being_touched() {
        // A principal with steady traffic must never be evicted no matter
        // how many sweeps run, because every `check` call refreshes
        // `last_seen` -- this is the property that makes eviction safe for
        // genuinely active principals, not just idle ones.
        let limiter = Limiter::new();
        let lim = limits(Some(10), None);
        let mut now = Instant::now();
        limiter.check(crate::snapshot::tid(1), &lim, 1, now);

        for _ in 0..5 {
            now += Duration::from_secs(30);
            limiter.check(crate::snapshot::tid(1), &lim, 1, now);
            limiter.evict_idle(now, Duration::from_secs(60));
            assert_eq!(
                limiter.entry_count(),
                1,
                "a principal touched every 30s must survive a 60s idle cutoff"
            );
        }
    }

    #[test]
    fn evict_idle_reclaims_the_right_principal_and_only_that_one() {
        let limiter = Limiter::new();
        let lim = limits(Some(10), None);
        let now = Instant::now();
        limiter.check(crate::snapshot::tid(1), &lim, 1, now);
        let later = now + Duration::from_secs(120);
        limiter.check(crate::snapshot::tid(2), &lim, 1, later);

        // Principal 1 has been idle for 120s at `later`; principal 2 was
        // just touched. A 60s cutoff must reclaim exactly principal 1.
        limiter.evict_idle(later, Duration::from_secs(60));
        assert_eq!(limiter.entry_count(), 1);
        assert!(
            limiter.share_of(crate::snapshot::tid(2)).is_some(),
            "the recently-touched principal must survive"
        );
        assert!(
            limiter.share_of(crate::snapshot::tid(1)).is_none(),
            "the long-idle principal must be gone"
        );
    }
}
