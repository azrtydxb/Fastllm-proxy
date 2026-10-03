//! Admission control: FastLLM queues so the engine does not have to.
//!
//! vLLM accepts every request offered to it and queues the surplus itself.
//! That queue is unbounded, invisible to the caller, and served without
//! priority, so a burst does not fail — it makes *every* request slow,
//! including the ones already running. A 35B on one GPU reached a 69% 502
//! rate this way: the queue grew, first-byte latency crossed the proxy's
//! upstream timeout, and the timeout path ejected a backend that was serving
//! perfectly well, just late.
//!
//! The fix is to keep the queue on this side of the wire, where it is bounded
//! and where a caller can be told "not now" in a millisecond instead of
//! waiting five minutes to be told nothing. The engine is fed only as much as
//! it is keeping up with; the rest waits here.
//!
//! ## Why the ceiling moves
//!
//! A fixed concurrency limit is wrong twice over: too low and a fast engine
//! idles, too high and a slow one drowns. The limit here is driven by what
//! the engine reports about itself — `vllm:num_requests_waiting`, already
//! scraped every tick by [`crate::engine_scrape`]. A queue that is growing is
//! the engine saying it is behind, whatever the cause: a long prefill, a
//! cold prefix cache, a neighbour stealing the GPU. Nothing here has to model
//! why.
//!
//! Multiplicative decrease, additive increase — the same asymmetry TCP uses
//! and for the same reason. Overload is expensive and compounding, so back
//! off fast; recovery is cheap to get wrong, so return slowly, or the ceiling
//! oscillates between drowning and idling.
//!
//! ## What this is not
//!
//! It is not a rate limit ([`crate::limiter`] is), and it is not a fairness
//! mechanism: permits are handed out in arrival order and a large request
//! costs exactly one, the same as a small one. It bounds how many requests
//! this replica has *in flight to one engine*, nothing else.
//!
//! ## Per replica, deliberately
//!
//! Each proxy holds its own ceiling, so N replicas admit up to N times it.
//! That is the same trade every other counter in `registry.rs` makes and for
//! the same reason: sharing it means a network round trip on the request
//! path, which is the one thing this codebase does not do. The feedback
//! signal absorbs the error — the engine's queue depth already reflects what
//! *every* replica sent, so each replica sees the consequence of the others
//! and backs off on it.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Permits the ceiling was lowered by while they were in use, and which are
/// therefore still owed. See `Admission::shrink`.
type Debt = Arc<AtomicUsize>;

/// How one backend's gate is tuned.
///
/// Set per backend -- on the `model_backends` row, through
/// `PATCH /admin/backends/{id}` and the Models page -- because the right
/// numbers are a property of the engine: a 35B on one GPU and an embedding
/// model on the same box want nothing like the same ceiling, and a hosted
/// provider wants no gate at all. A backend with no settings has no gate.
///
/// All `u32` so the whole thing can sit in the backend's identity key and on
/// the wire without conversions that could disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Settings {
    /// Most concurrent requests this replica sends the engine while it keeps
    /// up. The ceiling starts here and never grows past it.
    pub max_concurrent: u32,
    /// Engine-reported queue depth at or above which the ceiling halves.
    ///
    /// Not zero: an engine running at capacity holds a small queue at all
    /// times and that is healthy -- it is what keeps the GPU from idling
    /// between requests. Only a queue that keeps *growing* is a problem.
    pub high_water: u32,
    /// Callers allowed to wait here for a slot before new ones are refused.
    ///
    /// The bound is the point of it. An unbounded wait queue is vLLM's
    /// problem moved one process to the left.
    pub max_queued: u32,
    /// How long a caller waits for a slot before it is refused.
    pub max_wait_seconds: u32,
}

impl Settings {
    /// The ceiling never drops below one. Below that the backend is not slow,
    /// it is effectively down, and the health machinery decides its fate.
    const MIN_CONCURRENT: usize = 1;

    fn max_concurrent(&self) -> usize {
        (self.max_concurrent as usize).max(Self::MIN_CONCURRENT)
    }

    fn max_wait(&self) -> Duration {
        Duration::from_secs(u64::from(self.max_wait_seconds))
    }
}

/// Why a request was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// The wait queue was already full. Nothing waited.
    Full,
    /// Waited `max_wait` and no permit came free.
    TimedOut,
}

impl Rejected {
    /// What to put in `Retry-After`, in seconds.
    ///
    /// A guess either way, but a bounded one, and a caller that honours it
    /// stops adding to a queue that is already the problem.
    pub fn retry_after_seconds(&self) -> u64 {
        match self {
            // Nothing was learned about how long the backlog is, so the
            // shorter of the two: the queue may drain immediately.
            Rejected::Full => 5,
            // A full wait elapsed and nothing freed up. Longer.
            Rejected::TimedOut => 30,
        }
    }
}

/// The gate in front of one engine.
///
/// Held by a `Backend` and shared by every model that routes to it — the
/// engine is the thing being protected, not the name the caller used.
#[derive(Debug)]
pub struct Admission {
    /// Permits currently available *plus* those handed out. Shrunk by
    /// forgetting permits and grown by adding them, so the live ceiling is
    /// `capacity` and the semaphore enforces it without a lock.
    sem: Arc<Semaphore>,
    /// The ceiling as this replica currently believes it should be. Tracked
    /// separately because `Semaphore` exposes only what is *available*, which
    /// is the ceiling minus what is in flight.
    capacity: AtomicUsize,
    /// Callers blocked in `admit`. Compared against `max_queued` before a
    /// caller is allowed to join them.
    queued: AtomicUsize,
    /// Slots the ceiling has been lowered by that were in use at the time.
    ///
    /// `Semaphore::forget_permits` can only remove permits that are
    /// *available*, and an engine is overloaded precisely when none are: every
    /// slot is held by a request in progress. Forgetting alone would remove
    /// nothing at the one moment shrinking matters, and every held permit
    /// would come back at full strength when its request finished. So the
    /// shortfall is recorded here, and each returning [`Permit`] pays it down
    /// by being forgotten instead of returned. Shared with the permits, which
    /// is why it is an `Arc`.
    debt: Debt,
    /// Outcomes since this gate was created, for `/metrics` and the fleet
    /// report. A gate is created with its backend, so these reset when the
    /// backend's configuration changes -- the same lifetime every other
    /// per-backend counter has.
    admitted: AtomicU64,
    refused_full: AtomicU64,
    refused_timed_out: AtomicU64,
    settings: Settings,
}

/// What a gate is doing right now. Serialised into the fleet health report,
/// which is how the dashboard shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    /// The ceiling as configured.
    pub max_concurrent: u32,
    /// The ceiling now. Below `max_concurrent` means the engine's queue drove
    /// it down -- the gate is actively holding traffic back.
    pub capacity: u32,
    /// Slots held by requests in progress.
    pub in_use: u32,
    /// Callers waiting here for a slot.
    pub queued: u32,
    pub admitted_total: u64,
    /// Refused without waiting: the wait queue was already full.
    pub refused_full_total: u64,
    /// Refused after waiting `max_wait_seconds` for a slot.
    pub refused_timed_out_total: u64,
}

/// A slot at the gate, held for as long as the request is.
///
/// A wrapper rather than tokio's permit so that returning a slot can repay
/// the gate's `debt` instead of handing the slot to the next caller -- the
/// only moment a ceiling lowered under load can actually take effect.
#[derive(Debug)]
pub struct Permit {
    inner: Option<OwnedSemaphorePermit>,
    debt: Debt,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let Some(permit) = self.inner.take() else {
            return;
        };
        let repaid = self
            .debt
            .try_update(Ordering::AcqRel, Ordering::Acquire, |d| d.checked_sub(1))
            .is_ok();
        if repaid {
            // Out of circulation for good: this is the ceiling coming down.
            permit.forget();
        }
        // Otherwise `permit` drops here and goes back to the semaphore.
    }
}

impl Admission {
    /// A gate at full capacity.
    pub fn new(settings: Settings) -> Self {
        let ceiling = settings.max_concurrent();
        Self {
            sem: Arc::new(Semaphore::new(ceiling)),
            capacity: AtomicUsize::new(ceiling),
            queued: AtomicUsize::new(0),
            debt: Arc::new(AtomicUsize::new(0)),
            admitted: AtomicU64::new(0),
            refused_full: AtomicU64::new(0),
            refused_timed_out: AtomicU64::new(0),
            settings,
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    /// Everything `/metrics` and the fleet report show, read once.
    ///
    /// Not an atomic snapshot of the whole gate -- each figure is read on its
    /// own -- which is fine for a gauge sampled every few seconds and would
    /// cost a lock on the request path to do better.
    pub fn status(&self) -> Status {
        let capacity = self.capacity.load(Ordering::Relaxed);
        // Held = ceiling + what is still owed - what is free. See `debt`.
        let in_use = (capacity + self.debt.load(Ordering::Relaxed))
            .saturating_sub(self.sem.available_permits());
        Status {
            max_concurrent: self.settings.max_concurrent,
            capacity: capacity as u32,
            in_use: in_use as u32,
            queued: self.queued.load(Ordering::Relaxed) as u32,
            admitted_total: self.admitted.load(Ordering::Relaxed),
            refused_full_total: self.refused_full.load(Ordering::Relaxed),
            refused_timed_out_total: self.refused_timed_out.load(Ordering::Relaxed),
        }
    }

    /// Take a slot only if one is free right now.
    ///
    /// For a caller with somewhere else to go: queueing behind a saturated
    /// engine while a sibling in the same pool sits idle would be the gate
    /// defeating the load balancing it sits inside.
    pub fn try_admit(&self) -> Option<Permit> {
        Arc::clone(&self.sem)
            .try_acquire_owned()
            .ok()
            .map(|p| self.wrap(p))
    }

    /// Count a refusal. Separate from `admit` because a caller that moves on
    /// to a sibling after `try_admit` fails is refused *here* too, and
    /// `/metrics` should say so: that is the gate diverting traffic.
    pub fn note_refused(&self, why: Rejected) {
        match why {
            Rejected::Full => &self.refused_full,
            Rejected::TimedOut => &self.refused_timed_out,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn wrap(&self, permit: OwnedSemaphorePermit) -> Permit {
        self.admitted.fetch_add(1, Ordering::Relaxed);
        Permit {
            inner: Some(permit),
            debt: Arc::clone(&self.debt),
        }
    }

    /// Take a slot, waiting if the engine is busy.
    ///
    /// The permit is held for the life of the request — including the body
    /// stream, so a slow generation occupies its slot until the last token,
    /// which is exactly the pressure this exists to apply.
    pub async fn admit(&self) -> Result<Permit, Rejected> {
        // The fast path, and the common one: a permit is free, nobody waits,
        // and this costs one atomic compare-and-swap.
        if let Some(permit) = self.try_admit() {
            return Ok(permit);
        }

        // Check the bound *before* joining the queue. Checking after would
        // let every caller in and refuse them one timeout later, which is
        // the failure mode this whole module exists to avoid.
        if self.queued.load(Ordering::Relaxed) >= self.settings.max_queued as usize {
            self.note_refused(Rejected::Full);
            return Err(Rejected::Full);
        }

        self.queued.fetch_add(1, Ordering::Relaxed);
        // A guard, not a `fetch_sub` after the await: hyper drops the handler
        // future when a client hangs up, and a decrement that only runs on
        // the way out of the await never runs then. Every impatient client
        // would leak one `max_queued` slot for good, until the gate refused
        // everything with an idle engine behind it.
        let _queued = QueuedGuard(&self.queued);
        let outcome = tokio::time::timeout(
            self.settings.max_wait(),
            Arc::clone(&self.sem).acquire_owned(),
        )
        .await;

        match outcome {
            // `acquire_owned` errors only when the semaphore is closed, and
            // nothing here ever closes it.
            Ok(Ok(permit)) => Ok(self.wrap(permit)),
            Ok(Err(_)) | Err(_) => {
                self.note_refused(Rejected::TimedOut);
                Err(Rejected::TimedOut)
            }
        }
    }

    /// Feed the gate what the engine just said about itself.
    ///
    /// Called by [`crate::engine_scrape`] on its own timer, never from a
    /// request — the request path reads the consequence, not the metric.
    pub fn observe(&self, engine_waiting: u32) {
        if engine_waiting >= self.settings.high_water {
            self.shrink();
        } else if engine_waiting == 0 {
            // Only a genuinely drained queue earns capacity back. Growing
            // while anything is still waiting chases the ceiling back up
            // into the overload it just escaped.
            self.grow();
        }
    }

    /// Halve the ceiling, down to one.
    fn shrink(&self) {
        let now = self.capacity.load(Ordering::Relaxed);
        let target = (now / 2).max(Settings::MIN_CONCURRENT);
        if target >= now {
            return;
        }
        // Take what is free now; whatever is in use is owed, and comes out of
        // circulation as its request finishes (see `debt`). Either way no
        // running request loses its slot -- the engine is behind, and killing
        // work in progress would only put it further behind.
        let cut = now - target;
        let forgotten = self.sem.forget_permits(cut);
        self.debt.fetch_add(cut - forgotten, Ordering::AcqRel);
        self.capacity.store(target, Ordering::Relaxed);
    }

    /// Give one slot back, up to `max_concurrent`.
    fn grow(&self) {
        let now = self.capacity.load(Ordering::Relaxed);
        if now >= self.settings.max_concurrent() {
            return;
        }
        // A slot still owed from an earlier cut can simply stay in
        // circulation instead of being forgotten and re-added.
        let forgave = self
            .debt
            .try_update(Ordering::AcqRel, Ordering::Acquire, |d| d.checked_sub(1))
            .is_ok();
        if !forgave {
            self.sem.add_permits(1);
        }
        self.capacity.store(now + 1, Ordering::Relaxed);
    }

    /// The live ceiling. For `/health`, the fleet report and tests.
    pub fn capacity(&self) -> usize {
        self.capacity.load(Ordering::Relaxed)
    }

    /// Callers waiting for a permit right now.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }
}

/// Decrements the waiting count when dropped, however the wait ended.
struct QueuedGuard<'a>(&'a AtomicUsize);

impl Drop for QueuedGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            high_water: 4,
            max_concurrent: 8,
            max_queued: 2,
            // Zero: a caller that finds no slot is refused at once, so the
            // tests that are not about waiting do not spend time doing it.
            max_wait_seconds: 0,
        }
    }

    /// A caller that hangs up while waiting must give its queue slot back.
    /// Hyper cancels the handler by dropping it, which skips any code after
    /// the await -- the leak this fails against.
    #[tokio::test]
    async fn a_cancelled_wait_gives_back_its_queue_slot() {
        let gate = Arc::new(Admission::new(Settings {
            max_concurrent: 1,
            max_queued: 1,
            max_wait_seconds: 60,
            ..settings()
        }));
        let _held = gate.admit().await.unwrap();
        for _ in 0..3 {
            let g = Arc::clone(&gate);
            let waiter = tokio::spawn(async move { g.admit().await });
            tokio::time::sleep(Duration::from_millis(20)).await;
            waiter.abort();
            let _ = waiter.await;
        }
        assert_eq!(gate.queued(), 0, "aborted waiters left the count behind");
    }

    /// What the dashboard and `/metrics` read. Held slots, the moving
    /// ceiling and both kinds of refusal must each land in their own figure,
    /// or an operator cannot tell "throttled" from "refusing" from "idle".
    #[tokio::test]
    async fn status_reports_what_the_gate_is_doing() {
        let gate = Admission::new(Settings {
            max_concurrent: 2,
            max_queued: 0,
            ..settings()
        });
        let a = gate.admit().await.unwrap();
        let _b = gate.admit().await.unwrap();
        assert_eq!(gate.admit().await.unwrap_err(), Rejected::Full);
        gate.note_refused(Rejected::TimedOut);
        gate.observe(99);

        let s = gate.status();
        assert_eq!(s.max_concurrent, 2);
        assert_eq!(s.capacity, 1, "the engine's queue halved the ceiling");
        assert_eq!(s.in_use, 2, "both running requests keep their slots");
        assert_eq!(s.admitted_total, 2);
        assert_eq!(s.refused_full_total, 1);
        assert_eq!(s.refused_timed_out_total, 1);

        drop(a);
        assert_eq!(
            gate.status().in_use,
            1,
            "a returning slot repays the cut rather than showing as free"
        );
    }

    /// The asymmetry the module header argues for, as behaviour: one
    /// overloaded sample costs half the ceiling, one idle sample returns one
    /// slot. Recovering from a single spike must take many good samples.
    #[test]
    fn it_backs_off_fast_and_returns_slowly() {
        let gate = Admission::new(settings());
        assert_eq!(gate.capacity(), 8);

        gate.observe(9);
        assert_eq!(
            gate.capacity(),
            4,
            "an overloaded engine halves the ceiling"
        );

        gate.observe(0);
        assert_eq!(gate.capacity(), 5, "recovery is one slot at a time");
        for _ in 0..10 {
            gate.observe(0);
        }
        assert_eq!(gate.capacity(), 8, "and stops at max_concurrent");
    }

    /// A queue below the high-water mark is the engine working, not the
    /// engine drowning. Shrinking on it would starve a backend that is
    /// perfectly healthy and merely busy.
    #[test]
    fn a_small_queue_changes_nothing() {
        let gate = Admission::new(settings());
        gate.observe(9);
        let held = gate.capacity();

        // Under high_water but not drained: neither direction.
        gate.observe(3);
        assert_eq!(gate.capacity(), held);
    }

    #[test]
    fn it_never_shrinks_below_the_floor() {
        let gate = Admission::new(settings());
        for _ in 0..20 {
            gate.observe(100);
        }
        assert_eq!(gate.capacity(), 1);
    }

    #[tokio::test]
    async fn permits_are_returned_when_the_request_ends() {
        let gate = Admission::new(Settings {
            max_concurrent: 1,
            ..settings()
        });

        let first = gate
            .admit()
            .await
            .expect("first request gets the only slot");
        assert!(
            gate.admit().await.is_err(),
            "the slot is taken, so the next caller waits and then gives up"
        );
        drop(first);
        assert!(gate.admit().await.is_ok(), "and it frees on drop");
    }

    /// The bound, which is the whole point: past `max_queued` a caller is
    /// refused immediately rather than joining a queue that is itself the
    /// problem. `Full` and `TimedOut` are distinguished because only one of
    /// them means the caller waited.
    #[tokio::test]
    async fn a_full_wait_queue_refuses_without_waiting() {
        let gate = Arc::new(Admission::new(Settings {
            max_concurrent: 1,
            max_queued: 1,
            max_wait_seconds: 30,
            ..settings()
        }));

        let _held = gate.admit().await.expect("the only slot");

        // One waiter is allowed; park it so the queue is genuinely occupied.
        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.admit().await.map(|_| ()) }
        });
        while gate.queued() == 0 {
            tokio::task::yield_now().await;
        }

        let started = std::time::Instant::now();
        assert_eq!(
            gate.admit().await.unwrap_err(),
            Rejected::Full,
            "the second waiter is over the bound"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "and it was refused immediately, not after max_wait"
        );

        waiter.abort();
    }

    /// Shrinking must not revoke a slot from a request already running: the
    /// engine is behind, and killing work in progress makes it further behind.
    #[tokio::test]
    async fn shrinking_does_not_disturb_requests_in_flight() {
        let gate = Admission::new(settings());
        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(gate.admit().await.expect("all eight slots are free"));
        }

        gate.observe(99);
        assert_eq!(gate.capacity(), 4);
        assert_eq!(
            held.len(),
            8,
            "every in-flight request still holds its slot"
        );

        // The reduction lands as the permits come back, not before.
        drop(held);
        assert_eq!(gate.sem.available_permits(), 4);
    }

    /// The other half of the accounting: capacity restored while slots are
    /// still owed must cancel the debt, not add a permit on top of it -- or
    /// the ceiling ends up above `max_concurrent` once the requests finish.
    #[tokio::test]
    async fn growing_while_slots_are_owed_cancels_the_debt() {
        let gate = Admission::new(settings());
        let mut held = Vec::new();
        for _ in 0..8 {
            held.push(gate.admit().await.unwrap());
        }
        gate.observe(99); // 8 -> 4, all four owed
        gate.observe(0); // 4 -> 5, one of them forgiven
        assert_eq!(gate.capacity(), 5);

        drop(held);
        assert_eq!(
            gate.sem.available_permits(),
            5,
            "available permits must match the ceiling once everything is back"
        );
    }
}
