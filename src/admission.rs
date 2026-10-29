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
//! ## One halving per cooldown, not one per sample
//!
//! The decrease is multiplicative but not *repeatable at scrape rate*. The
//! queue is engine-wide: both proxy replicas count against the same
//! `num_requests_waiting`, so a queue that merely stays deep (a cold prefix
//! cache, a neighbour on the GPU) reads as overload on every 2s scrape for as
//! long as it lasts. Halving on each of those − 8 → 4 → 2 → 1 in about six
//! seconds − drives every replica to a single slot on a queue that was never
//! growing, and the boxes sit underused while recovery crawls back additively.
//!
//! So every halving, without exception, needs [`SHRINK_COOLDOWN`] to have
//! passed since the previous one. The rule is on the *clock*, not on an
//! "episode" boundary, on purpose: an earlier version let any reading below
//! `high_water` end the episode, and a queue flickering 2,1,2,1 around
//! `high_water = 2` then reopened an episode on every other scrape and took the
//! gate 8 → 4 → 2 → 1 in the same six seconds. A flicker is exactly what a
//! queue that is merely deep looks like, so it cannot be allowed to reset the
//! brake. A genuinely worsening engine still gets backed off, at a deliberate
//! pace of one halving per cooldown. Recovery is separate: only a queue
//! drained to zero grows the ceiling back, one slot per scrape.
//!
//! ## What this is not
//!
//! It is not a rate limit ([`crate::limiter`] is), and it is not a fairness
//! mechanism: permits are handed out in arrival order and a large request
//! costs exactly one, the same as a small one. It bounds how many requests
//! this replica has *in flight to one engine*, nothing else.
//!
//! ## On by default
//!
//! Every backend is [`Config::Auto`] unless an operator says otherwise, so the
//! gate is not something anyone has to remember to switch on -- the deployment
//! that most needs it is the one with nobody watching. `Auto` gates only what
//! [`crate::engine_scrape`] has confirmed is a self-hosted vLLM/SGLang engine,
//! so a hosted provider is never gated no matter what it answers, and a backend
//! that has not been probed yet simply has no gate yet. The numbers come from
//! [`auto_settings`]: the measured per-box session limit divided across
//! replicas, lowered further only if the engine advertises a smaller batch.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Permits the ceiling was lowered by while they were in use, and which are
/// therefore still owed. See `Admission::shrink`.
type Debt = Arc<AtomicUsize>;

/// How one backend's gate is tuned.
///
/// Set per backend -- on the `model_backends` row, through
/// `PATCH /admin/backends/{id}` and the Models page. A backend defaults to
/// [`Config::Auto`]: the gate turns itself on, with [`auto_settings`], once the
/// scraper confirms the backend is a self-hosted vLLM/SGLang engine, and never
/// for a hosted provider. An operator overrides the numbers with
/// [`Config::Explicit`], or disables the gate outright with [`Config::Off`].
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

/// Persisted per-backend choice, the `admission_mode` column.
///
/// The distinction the column exists for is `Auto` versus `Off`: both mean "no
/// numbers of my own", but one wants a gate the moment an engine is detected
/// and the other never wants one. An older `null` `admission_max_concurrent`
/// could not tell them apart, which is why `Auto` is the default and a fresh
/// backend is auto-gated rather than left ungated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(tag = "mode", content = "settings", rename_all = "snake_case")]
pub enum Config {
    #[default]
    Auto,
    Explicit(Settings),
    Off,
}

/// Proxy replicas assumed when nothing says otherwise, used to divide a
/// box-wide engine capacity into a per-replica ceiling.
///
/// Two is what `deploy/proxy.yaml` runs for the two DGX Sparks serving
/// Qwen3.6-35B. The proxy cannot discover its own replica count without a
/// network call (and the request path makes none), so the operator states it
/// with `--admission-replicas` / `FASTLLM_ADMISSION_REPLICAS`, which must match
/// the number of proxy pods or the combined admission drifts from the box:
/// too low and the box is over-admitted, too high and it idles.
pub const DEFAULT_REPLICAS: u32 = 2;

/// Concurrency one DGX box comfortably serves for the 35B, measured by the
/// operator. Used both as the automatic ceiling when the engine reports no
/// capacity *and* as a hard cap on a reported one -- see [`auto_settings`].
pub const BOX_SESSIONS: u32 = 16;

/// Starting defaults for an engine's automatic gate at [`DEFAULT_REPLICAS`];
/// [`auto_settings`] recomputes the ceiling for the configured replica count.
///
/// `max_concurrent` is [`BOX_SESSIONS`] over [`DEFAULT_REPLICAS`]: 16 sessions is the
/// measured comfortable limit for the 35B on one DGX box, and each of the two
/// replicas holds its own share, so 8 each admits 16 between them. `high_water`
/// is 2: an engine at capacity holds a small queue at all times and that is
/// healthy, so only a queue that keeps growing should halve the ceiling. 32
/// local waiters and a 5s wait bound proxy memory and caller latency without
/// making a caller wait longer than a first byte typically takes.
pub const AUTO_FALLBACK: Settings = Settings {
    max_concurrent: per_replica_ceiling(BOX_SESSIONS, DEFAULT_REPLICAS),
    high_water: 2,
    max_queued: 32,
    max_wait_seconds: 5,
};

/// How long one halving must stand before the gate may halve again while the
/// engine's queue stays deep.
///
/// Ten seconds is deliberately several scrape intervals (the engine scraper
/// ticks every 2s by default, so five of them at the default rate) on purpose:
/// the queue depth is engine-wide, and both replicas see the same number, so a
/// queue that is merely deep is not evidence the ceiling is still too high.
/// Without the pause, back-to-back scrapes halve 8 → 4 → 2 → 1 in about six
/// seconds on a queue that never grew, pinning every replica at one slot and
/// leaving the box idle. One halving per cooldown, then this much patience,
/// spends the decrease where a *worsening* engine earns it instead of where a
/// steady queue merely repeats itself. The number is several scrapes' worth so
/// it cannot fire twice off one stretch of queue that has not had time to
/// respond to the first cut.
pub const SHRINK_COOLDOWN: Duration = Duration::from_secs(10);

/// Divide a box-wide target among proxy replicas, rounding up so odd targets
/// do not leave one session unused. At least one slot remains per replica.
pub const fn per_replica_ceiling(box_capacity: u32, replicas: u32) -> u32 {
    let replicas = if replicas < 1 { 1 } else { replicas };
    box_capacity.saturating_add(replicas - 1) / replicas
}

/// The automatic gate for an engine, given what its `/metrics` reported about
/// scheduler capacity (`vLLM max_num_seqs`, `SGLang max_running_requests`).
///
/// The reported figure is vLLM's *batching* ceiling, not the number of sessions
/// the box serves well: its default `max_num_seqs` is 256, so trusting it
/// directly would admit twenty times the measured comfortable limit and put the
/// engine straight back into the overload this gate exists to prevent. It is
/// therefore taken as an *upper bound* only -- the ceiling is the smaller of the
/// engine's own capacity over `replicas` and the measured per-replica default.
/// `replicas` is the deployment's proxy replica count (`--admission-replicas`).
/// An engine that reports a genuinely small batch (say 24) still lowers the
/// ceiling; one that reports the stock 256 leaves it at the safe default.
pub fn auto_settings(engine_capacity: Option<u32>, replicas: u32) -> Settings {
    let mut settings = AUTO_FALLBACK;
    settings.max_concurrent = per_replica_ceiling(BOX_SESSIONS, replicas);
    if let Some(capacity) = engine_capacity {
        // A capacity of 0 means "not advertised"; never gate to nothing.
        if capacity > 0 {
            settings.max_concurrent = settings
                .max_concurrent
                .min(per_replica_ceiling(capacity, replicas));
        }
    }
    settings
}

#[cfg(test)]
mod config_tests {
    use super::{
        auto_settings, per_replica_ceiling, Config, Settings, AUTO_FALLBACK, BOX_SESSIONS,
        DEFAULT_REPLICAS,
    };

    #[test]
    fn automatic_explicit_and_off_states_round_trip_over_snapshot_json() {
        let explicit = Settings {
            max_concurrent: 11,
            high_water: 2,
            max_queued: 32,
            max_wait_seconds: 5,
        };
        for state in [Config::Auto, Config::Explicit(explicit), Config::Off] {
            let encoded = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<Config>(&encoded).unwrap(), state);
        }
    }

    /// The automatic ceiling is replica-aware: a 16-session box split across
    /// two replicas admits 8 each, and an odd target rounds up so no session
    /// goes unused.
    #[test]
    fn automatic_ceiling_divides_the_box_across_replicas() {
        assert_eq!(per_replica_ceiling(BOX_SESSIONS, 2), 8);
        assert_eq!(per_replica_ceiling(17, 2), 9);
        // A degenerate replica count cannot divide by zero.
        assert_eq!(per_replica_ceiling(8, 0), 8);
        assert_eq!(AUTO_FALLBACK.max_concurrent, 8);
    }

    /// vLLM's `max_num_seqs` is a batching ceiling (stock default 256), not the
    /// number of sessions the box serves well. A reported figure may lower the
    /// ceiling but must never raise it above the measured per-replica default,
    /// or the automatic gate would re-create the overload it exists to stop.
    #[test]
    fn engine_capacity_only_lowers_the_automatic_ceiling() {
        assert_eq!(
            auto_settings(Some(24), DEFAULT_REPLICAS).max_concurrent,
            8.min(per_replica_ceiling(24, 2)),
            "a small engine batch lowers the ceiling"
        );
        assert_eq!(auto_settings(Some(24), DEFAULT_REPLICAS).max_concurrent, 8);
        assert_eq!(
            auto_settings(Some(256), DEFAULT_REPLICAS).max_concurrent,
            AUTO_FALLBACK.max_concurrent,
            "the stock vLLM batch does not raise the ceiling"
        );
        assert_eq!(
            auto_settings(Some(4), DEFAULT_REPLICAS).max_concurrent,
            2,
            "tiny batch honoured"
        );
        assert_eq!(
            auto_settings(Some(0), DEFAULT_REPLICAS).max_concurrent,
            AUTO_FALLBACK.max_concurrent,
            "an unset capacity is not a gate to zero"
        );
        assert_eq!(auto_settings(None, DEFAULT_REPLICAS), AUTO_FALLBACK);
    }

    /// The replica count is configuration, not a constant: the same box split
    /// across more proxies admits less per proxy, and one proxy takes it all.
    #[test]
    fn the_automatic_ceiling_follows_the_configured_replica_count() {
        assert_eq!(auto_settings(None, 1).max_concurrent, 16);
        assert_eq!(auto_settings(None, 2).max_concurrent, 8);
        assert_eq!(auto_settings(None, 4).max_concurrent, 4);
        assert_eq!(
            auto_settings(Some(8), 4).max_concurrent,
            2,
            "the engine's own capacity is divided by the same count"
        );
        assert_eq!(auto_settings(None, 0).max_concurrent, 16, "never divides by zero");
    }
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
    /// How many pre-gate requests were seeded as occupied slots and still owe
    /// their slot back. See `new_with_in_flight` and `release_seed`.
    seeded: AtomicUsize,
    /// When the ceiling was last halved; `None` before the first halving.
    /// Guards the cooldown -- see [`SHRINK_COOLDOWN`] and `observe_at`. A
    /// `Mutex` so the check and the stamp are one step: two observers racing
    /// past a separate load and store could both halve. Only the scraper
    /// touches it, never the request path.
    last_halved: Mutex<Option<Instant>>,
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
            seeded: AtomicUsize::new(0),
            last_halved: Mutex::new(None),
            settings,
        }
    }

    /// A gate installed on a backend that already has work in flight -- the
    /// live swap from no gate to an automatic one.
    ///
    /// Those requests were admitted before the gate existed and hold no permit,
    /// so they are seeded as occupied slots: the semaphore starts with
    /// `ceiling - seeded` free rather than the full ceiling, and each one
    /// releases its slot via [`release_seed`](Self::release_seed) on drop.
    /// Without this the gate would hand out a *full* ceiling on top of the
    /// requests already running, which is the overload it exists to prevent.
    /// Capped at the ceiling: work beyond it is not seeded, and its release is
    /// a no-op, so nothing leaks.
    pub fn new_with_in_flight(settings: Settings, in_flight: usize) -> Self {
        let ceiling = settings.max_concurrent();
        let seeded = in_flight.min(ceiling);
        let mut gate = Self::new(settings);
        gate.sem = Arc::new(Semaphore::new(ceiling - seeded));
        gate.seeded.store(seeded, Ordering::Relaxed);
        gate
    }

    /// A seeded, pre-gate request has finished: give its borrowed slot back.
    ///
    /// A no-op once `seeded` is exhausted, which is how the requests beyond the
    /// ceiling (never seeded) are ignored. Repays debt first, like every other
    /// returned slot, so a ceiling that came down under load is not lifted
    /// again by a request that never counted against it.
    pub fn release_seed(&self) {
        if self
            .seeded
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
            let repaid = self
                .debt
                .try_update(Ordering::AcqRel, Ordering::Acquire, |d| d.checked_sub(1))
                .is_ok();
            if !repaid {
                self.sem.add_permits(1);
            }
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
        self.observe_at(engine_waiting, Instant::now());
    }

    /// [`observe`](Self::observe) with the clock injected, so tests can step
    /// time across the cooldown deterministically without sleeping.
    ///
    /// A reading at or above `high_water` halves the ceiling only if
    /// [`SHRINK_COOLDOWN`] has passed since the last halving (or there has
    /// been none). The cooldown is the whole rule: readings below `high_water`
    /// do not reset it, because a queue flickering around the threshold is a
    /// queue that is merely deep, and letting each dip re-arm the brake is how
    /// the gate went 8 → 4 → 2 → 1 in six seconds. A queue drained to exactly
    /// zero earns one slot of capacity back; anything between zero and
    /// `high_water` changes nothing.
    pub fn observe_at(&self, engine_waiting: u32, now: Instant) {
        if engine_waiting >= self.settings.high_water {
            self.consider_shrink(now);
        } else if engine_waiting == 0 {
            // Only a genuinely drained queue earns capacity back. Growing
            // while anything is still waiting chases the ceiling back up
            // into the overload it just escaped.
            self.grow();
        }
    }

    /// Halve the ceiling if the cooldown since the last halving has passed.
    fn consider_shrink(&self, now: Instant) {
        let mut last = self
            .last_halved
            .lock()
            .expect("admission shrink lock poisoned");
        let due = match *last {
            Some(at) => now.saturating_duration_since(at) >= SHRINK_COOLDOWN,
            None => true,
        };
        if !due {
            return;
        }
        *last = Some(now);
        drop(last);
        self.shrink();
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

    /// Put the ceiling back at `max_concurrent` and forget the cooldown.
    ///
    /// For when the signal that drove the ceiling down is gone and will not be
    /// back to bring it up: the engine restarted, lost `/metrics`, or is no
    /// longer detected as an engine. Additive recovery needs readings of an
    /// empty queue, and without scrapes there are none, so a gate shrunk to one
    /// slot would stay there for good. Runs `grow` per missing slot rather than
    /// storing a number, so debt owed by requests still in flight is forgiven
    /// the same way it is on ordinary recovery and no slot is double counted.
    /// Requests already admitted are untouched.
    pub fn restore_full(&self) {
        while self.capacity.load(Ordering::Relaxed) < self.settings.max_concurrent() {
            self.grow();
        }
        let mut last = self
            .last_halved
            .lock()
            .expect("admission shrink lock poisoned");
        *last = None;
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

    /// The asymmetry the module header argues for, as behaviour: an overloaded
    /// sample halves the ceiling once, and recovery from that one cut is one
    /// slot per drained sample. Repeated deep samples inside the cooldown do
    /// not halve again -- tested separately.
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
    /// A gate shrunk by overload must be able to come back without any further
    /// readings, and requests already running must neither be cut off nor
    /// double-counted when it does.
    #[test]
    fn restore_full_returns_the_whole_ceiling_without_readings() {
        let gate = Admission::new(settings());
        let start = Instant::now();
        let held: Vec<_> = (0..8).map(|_| gate.try_admit().unwrap()).collect();
        gate.observe_at(9, start);
        gate.observe_at(9, start + SHRINK_COOLDOWN);
        assert_eq!(gate.capacity(), 2, "shrunk with every slot in use");

        gate.restore_full();
        assert_eq!(gate.capacity(), 8, "back at the configured ceiling");
        assert_eq!(gate.status().in_use, 8, "running requests still count");
        assert!(gate.try_admit().is_none(), "no slot is double counted");
        drop(held);
        assert_eq!(gate.status().in_use, 0);
        let again: Vec<_> = (0..8).filter_map(|_| gate.try_admit()).collect();
        assert_eq!(again.len(), 8, "all eight slots are usable, no more");
        assert!(gate.try_admit().is_none());

        // And the cooldown is forgotten: the next deep queue halves at once.
        drop(again);
        gate.observe_at(9, start + SHRINK_COOLDOWN + Duration::from_secs(1));
        assert_eq!(gate.capacity(), 4);
    }

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
        let start = Instant::now();
        // Cross the cooldown repeatedly so every observe that *may* halve
        // does, and the ceiling still cannot go below one.
        for step in 0..20 {
            gate.observe_at(100, start + SHRINK_COOLDOWN * step);
        }
        assert_eq!(
            gate.capacity(),
            1,
            "the floor is one, however deep the queue"
        );
    }

    /// The point of the change: a queue that simply *stays* deep is not
    /// evidence to halve again, because that queue is engine-wide and repeats
    /// on every scrape. Many observes inside the cooldown must produce one cut.
    #[test]
    fn one_cooldown_halves_only_once_however_many_observes() {
        let gate = Admission::new(settings());
        let start = Instant::now();

        // The crossing halves at once...
        gate.observe_at(9, start);
        assert_eq!(gate.capacity(), 4, "the first deep reading halves");

        // ...and stays halved for the whole cooldown, even one scrape short of
        // the cooldown, however many samples arrive.
        for step in 1..5 {
            gate.observe_at(9, start + Duration::from_secs(step));
        }
        assert_eq!(
            gate.capacity(),
            4,
            "still-deep scrapes inside the cooldown must not halve again"
        );
    }

    /// A queue that is *still* deep after the cooldown has had time to answer
    /// the first cut gets one more halving -- and only one.
    #[test]
    fn a_still_deep_queue_halves_again_after_the_cooldown() {
        let gate = Admission::new(settings());
        let start = Instant::now();
        gate.observe_at(9, start);
        assert_eq!(gate.capacity(), 4);

        // Just before the cooldown: nothing.
        gate.observe_at(9, start + SHRINK_COOLDOWN - Duration::from_secs(1));
        assert_eq!(gate.capacity(), 4, "cooldown not yet elapsed");

        // At the cooldown: one further halving, and the cooldown restarts.
        gate.observe_at(9, start + SHRINK_COOLDOWN);
        assert_eq!(gate.capacity(), 2, "a worsening engine still backs off");

        // The restarted cooldown holds until it too elapses.
        gate.observe_at(9, start + SHRINK_COOLDOWN + Duration::from_secs(1));
        assert_eq!(gate.capacity(), 2, "the restarted cooldown applies");
        gate.observe_at(9, start + SHRINK_COOLDOWN * 2);
        assert_eq!(gate.capacity(), 1, "then one more, down to the floor");
    }

    /// The cooldown is on the clock, not on episode boundaries: a queue that
    /// drains and returns inside the cooldown must not halve again. A drained
    /// reading still earns its slot of recovery.
    #[test]
    fn a_dip_and_return_inside_the_cooldown_does_not_halve_again() {
        let gate = Admission::new(settings());
        let start = Instant::now();
        gate.observe_at(9, start);
        assert_eq!(gate.capacity(), 4);

        gate.observe_at(0, start + Duration::from_secs(1));
        assert_eq!(gate.capacity(), 5, "recovery still proceeds additively");

        gate.observe_at(9, start + Duration::from_secs(2));
        assert_eq!(
            gate.capacity(),
            5,
            "a fresh crossing inside the cooldown is not a new halving"
        );

        gate.observe_at(9, start + SHRINK_COOLDOWN);
        assert_eq!(gate.capacity(), 2, "after the cooldown it halves again");
    }

    /// A queue flickering around `high_water` (2,1,2,1 with high_water = 2) is
    /// the shape of a queue that is merely deep. It used to reopen an episode
    /// every other scrape and take 8 -> 4 -> 2 -> 1 in about six seconds. At
    /// most one halving per cooldown, however it flickers.
    #[test]
    fn an_oscillating_queue_halves_at_most_once_per_cooldown() {
        let gate = Admission::new(Settings {
            high_water: 2,
            ..settings()
        });
        let start = Instant::now();

        // Twelve seconds of scrapes every second, alternating 2,1,2,1...
        // The first reading halves 8 -> 4 at t=0; the next opportunity is t=10.
        let mut halvings = 0;
        let mut last = gate.capacity();
        for sec in 0..=12u64 {
            let waiting = if sec % 2 == 0 { 2 } else { 1 };
            gate.observe_at(waiting, start + Duration::from_secs(sec));
            if gate.capacity() < last {
                halvings += 1;
            }
            last = gate.capacity();
        }
        assert_eq!(
            halvings, 2,
            "t=0 and t=10 only: one halving per cooldown, not per crossing"
        );
        assert_eq!(gate.capacity(), 2, "8 -> 4 -> 2, never down to the floor");

        // Within any window shorter than the cooldown there is never a second cut.
        let gate = Admission::new(Settings {
            high_water: 2,
            ..settings()
        });
        for sec in 0..10u64 {
            let waiting = if sec % 2 == 0 { 2 } else { 1 };
            gate.observe_at(waiting, start + Duration::from_secs(sec));
        }
        assert_eq!(gate.capacity(), 4, "six seconds of flicker cost one halving");
    }

    /// The live swap from "no gate" to "auto gate": in-flight requests must
    /// start out consuming slots, so the gate never admits a full ceiling on
    /// top of work the engine is already doing, and their slots must come back
    /// when they finish rather than leak.
    #[tokio::test]
    async fn an_install_counts_in_flight_work_and_returns_its_slots() {
        let gate = Admission::new_with_in_flight(settings(), 3);
        assert_eq!(
            gate.capacity(),
            8,
            "the ceiling is unchanged by the install"
        );
        assert_eq!(
            gate.status().in_use,
            3,
            "three pre-gate requests are counted"
        );

        // Only five slots are free until the pre-gate work finishes.
        let mut fresh = Vec::new();
        for _ in 0..5 {
            fresh.push(gate.try_admit().expect("five slots remain"));
        }
        assert!(
            gate.try_admit().is_none(),
            "the gate must not hand out more than ceiling minus in-flight"
        );

        gate.release_seed();
        assert!(
            gate.try_admit().is_some(),
            "a pre-gate request ending frees its slot"
        );

        // Releasing more seeds than were seeded is a no-op: work beyond the
        // ceiling was never counted and must not inflate the ceiling.
        for _ in 0..10 {
            gate.release_seed();
        }
        drop(fresh);
        assert_eq!(
            gate.sem.available_permits(),
            8,
            "the ceiling is exactly max_concurrent once everything is back"
        );
    }

    /// A pool of in-flight work larger than the ceiling seeds only up to the
    /// ceiling; the excess is not owed, so its ending cannot push permits
    /// above `max_concurrent`.
    #[tokio::test]
    async fn more_in_flight_than_the_ceiling_cannot_over_release() {
        let gate = Admission::new_with_in_flight(settings(), 20);
        assert_eq!(gate.status().in_use, 8, "seeded up to the ceiling");
        for _ in 0..50 {
            gate.release_seed();
        }
        assert_eq!(
            gate.sem.available_permits(),
            8,
            "releasing un-seeded work must not add permits"
        );
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
