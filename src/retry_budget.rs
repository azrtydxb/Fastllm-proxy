//! A cap on how much of the load may be retries.
//!
//! A backend that is slow rather than dead makes every request to it wait out
//! its timeout and then retry onto a sibling, so the siblings receive the
//! failing backend's traffic on top of their own at the moment the fleet can
//! least afford it. Each retry is individually reasonable and together they
//! are how a partial slowdown becomes a total one.
//!
//! The rule is the usual one: retries may be a fixed fraction of recent
//! requests, plus a small floor so a quiet system can still retry at all. A
//! request refused a retry is answered with what it already got, which is the
//! honest outcome and lets the caller's own backoff do the spacing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Window length in seconds. Long enough to smooth a burst, short enough that
/// the budget follows the load.
const WINDOW_SECS: u64 = 10;

/// Retries allowed per request in the window, as a fraction (1/5).
const RATIO_DENOM: u64 = 5;

/// Retries always allowed per window, so a low-traffic system is not starved.
const FLOOR: u64 = 10;

#[derive(Default)]
pub struct RetryBudget {
    window: AtomicU64,
    requests: AtomicU64,
    retries: AtomicU64,
}

impl RetryBudget {
    fn roll(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            / WINDOW_SECS;
        if self.window.swap(now, Ordering::Relaxed) != now {
            // Approximate on purpose: two threads rolling at once may each
            // clear a little of the other's count, which only ever loosens
            // the budget for one window.
            self.requests.store(0, Ordering::Relaxed);
            self.retries.store(0, Ordering::Relaxed);
        }
    }

    /// Count one client request.
    pub fn note_request(&self) {
        self.roll();
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether one more retry fits. Consumes budget when it does.
    pub fn allow(&self) -> bool {
        self.roll();
        let allowed = FLOOR + self.requests.load(Ordering::Relaxed) / RATIO_DENOM;
        self.retries.fetch_add(1, Ordering::Relaxed) < allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_are_capped_to_a_fraction_of_requests() {
        let b = RetryBudget::default();
        for _ in 0..100 {
            b.note_request();
        }
        // 100 requests: 10 floor + 20 by ratio.
        let allowed = (0..200).filter(|_| b.allow()).count();
        assert_eq!(allowed, 30);
    }

    #[test]
    fn a_quiet_system_can_still_retry() {
        let b = RetryBudget::default();
        b.note_request();
        assert!(b.allow(), "the floor keeps a lone request retryable");
    }
}
