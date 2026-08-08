//! A latency histogram that can be written from the request path.
//!
//! # Why not a general-purpose histogram crate
//!
//! Everything on this path is measured against a budget of well under a
//! microsecond of the proxy's own work per request (`bench/micro`, and
//! `docs/performance.md`). That rules out anything which allocates, takes a
//! lock, or resizes: the cost has to be a bucket index and a couple of atomic
//! adds, and it has to stay that way when eight threads are recording at once.
//!
//! So: fixed boundaries chosen up front, a flat array of counters, relaxed
//! atomics, no allocation after construction, and no code path that can block.
//!
//! # Why explicit boundaries rather than powers of two
//!
//! Powers of two are one instruction to index (`leading_zeros`) but put a 2x
//! error bar on every quantile. For the number this proxy is judged on — time
//! to first token, where the difference between 165ms and 330ms is the whole
//! argument — that is not good enough. The boundaries below are the
//! conventional Prometheus ladder, dense where this workload actually lives
//! and sparse in the tail, and finding the bucket is a short forward scan over
//! a cache-resident array.

use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bounds in **microseconds**, ascending. Anything larger lands in the
/// implicit `+Inf` bucket.
///
/// Dense between a millisecond and a few seconds because that is where both
/// time-to-first-token and whole non-streaming requests fall; the sub-
/// millisecond end still matters because work this proxy does itself (routing,
/// authorisation, classification) is measured there.
pub const BOUNDS_US: &[u64] = &[
    100,        // 0.1 ms — proxy-internal work
    250,        // 0.25
    500,        // 0.5
    1_000,      // 1 ms
    2_500,      // 2.5
    5_000,      // 5
    10_000,     // 10
    25_000,     // 25
    50_000,     // 50
    100_000,    // 100 ms — a fast TTFT
    250_000,    // 250
    500_000,    // 500
    1_000_000,  // 1 s
    2_500_000,  // 2.5
    5_000_000,  // 5
    10_000_000, // 10
    30_000_000, // 30
    60_000_000, // 60 s
];

/// One bucket per bound, plus `+Inf`.
const N: usize = BOUNDS_US.len() + 1;

/// A fixed-bucket histogram over microsecond values.
#[derive(Debug)]
pub struct Histogram {
    buckets: [AtomicU64; N],
    /// Microseconds, summed. Prometheus wants a `_sum` to compute averages
    /// from, and it also lets a scrape spot values that saturated `+Inf`.
    ///
    /// There is deliberately no `count` field: the total is the sum of the
    /// buckets, which a scrape can add up in nanoseconds, and keeping it as a
    /// third atomic cost more than it sounds. Every thread hits a `count` on
    /// one cache line on every observation, where bucket writes at least
    /// spread across the array. Measured on this machine at 8 threads:
    /// 91ns/record with it, 58ns without (`bench/micro`).
    sum_us: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub const fn new() -> Self {
        // `AtomicU64::new(0)` is not `Copy`, so the array cannot be built with
        // `[expr; N]`. A const initialiser keeps this usable in a `static`.
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Self {
            buckets: [ZERO; N],
            sum_us: AtomicU64::new(0),
        }
    }

    /// Record one observation.
    ///
    /// `Relaxed` throughout: these counters are read by a scrape that has no
    /// happens-before relationship with any request, and imposing one would
    /// buy ordering nobody consumes at the cost of a barrier on the hot path.
    /// A scrape landing mid-update can see a count that does not yet match the
    /// bucket sum, which is the same tolerance every Prometheus client has.
    #[inline]
    pub fn record_us(&self, value_us: u64) {
        let idx = bucket_index(value_us);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(value_us, Ordering::Relaxed);
    }

    /// Total observations, including those past the last bound.
    ///
    /// Summed on read rather than counted on write — see [`Self::sum_us`]'s
    /// field for why. A scrape adding 19 numbers is free; a third atomic on
    /// every request is not.
    pub fn count(&self) -> u64 {
        self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).sum()
    }

    pub fn sum_us(&self) -> u64 {
        self.sum_us.load(Ordering::Relaxed)
    }

    /// Cumulative counts for the **finite** buckets, one per entry in
    /// [`BOUNDS_US`], as Prometheus defines them: each `le` bucket includes
    /// everything below it.
    ///
    /// Deliberately excludes the `+Inf` bucket. Prometheus's `+Inf` is the
    /// total observation count, which [`Self::count`] already is — returning
    /// it here as well invites a caller to render the overflow twice, or to
    /// take `last()` believing it is the last finite bound.
    pub fn cumulative(&self) -> Vec<u64> {
        let mut running = 0;
        self.buckets[..BOUNDS_US.len()]
            .iter()
            .map(|b| {
                running += b.load(Ordering::Relaxed);
                running
            })
            .collect()
    }

    /// Render as a Prometheus histogram, in **seconds** — the unit Prometheus
    /// convention requires for durations, so `histogram_quantile` and every
    /// dashboard that consumes it work without a scale factor.
    ///
    /// `labels` is rendered inside the brace pair and must not include `le`.
    pub fn render(&self, out: &mut String, name: &str, labels: &str) {
        let sep = if labels.is_empty() { "" } else { "," };
        for (bound, cum) in BOUNDS_US.iter().zip(self.cumulative()) {
            out.push_str(&format!(
                "{name}_bucket{{{labels}{sep}le=\"{}\"}} {cum}\n",
                seconds(*bound)
            ));
        }
        let total = self.count();
        out.push_str(&format!(
            "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {total}\n"
        ));
        out.push_str(&format!(
            "{name}_sum{{{labels}}} {}\n",
            seconds(self.sum_us())
        ));
        out.push_str(&format!("{name}_count{{{labels}}} {total}\n"));
    }
}

/// Microseconds as fixed-point seconds, without a float round trip.
///
/// Formatting through `f64` would print `0.0001` as `0.00009999999999999999`
/// for some values, and a bucket boundary that does not match the one a
/// dashboard was built against silently produces no data.
fn seconds(us: u64) -> String {
    format!("{}.{:06}", us / 1_000_000, us % 1_000_000)
}

/// Index of the first bucket whose bound is `>= value_us`, or `+Inf`.
///
/// A forward scan rather than a binary search: the array is 18 entries in one
/// or two cache lines, and most real values land in the first half of it, so
/// the scan usually stops early and never mispredicts the way a search would.
#[inline]
fn bucket_index(value_us: u64) -> usize {
    let mut i = 0;
    while i < BOUNDS_US.len() {
        if value_us <= BOUNDS_US[i] {
            return i;
        }
        i += 1;
    }
    BOUNDS_US.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_lands_in_the_first_bucket_it_fits() {
        // Boundaries are inclusive upper bounds, which is what Prometheus's
        // `le` means. Getting this off by one puts every value that lands
        // exactly on a boundary — and round numbers are common — in the wrong
        // bucket.
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(100), 0, "exactly on a bound is inside it");
        assert_eq!(bucket_index(101), 1);
        assert_eq!(bucket_index(60_000_000), BOUNDS_US.len() - 1);
        assert_eq!(bucket_index(60_000_001), BOUNDS_US.len(), "+Inf");
        assert_eq!(bucket_index(u64::MAX), BOUNDS_US.len());
    }

    #[test]
    fn bounds_ascend_so_the_scan_is_correct() {
        assert!(
            BOUNDS_US.windows(2).all(|w| w[0] < w[1]),
            "a non-ascending ladder would make the forward scan return the \
             wrong bucket and Prometheus reject the series"
        );
    }

    #[test]
    fn buckets_are_cumulative_as_prometheus_requires() {
        let h = Histogram::new();
        h.record_us(50); // bucket 0
        h.record_us(300); // bucket 2 (<= 500)
        h.record_us(90_000_000); // +Inf

        let cum = h.cumulative();
        assert_eq!(cum[0], 1);
        assert_eq!(cum[1], 1, "nothing between 100 and 250");
        assert_eq!(cum[2], 2, "cumulative, so it carries bucket 0 forward");
        assert_eq!(
            *cum.last().unwrap(),
            2,
            "the last finite bucket must not include the +Inf observation"
        );
        assert_eq!(h.count(), 3, "but the total does");
        assert_eq!(h.sum_us(), 50 + 300 + 90_000_000);
    }

    /// The `+Inf` bucket is `count`, not the last finite bucket. Emitting the
    /// finite total there loses every observation past the ladder, which is
    /// exactly the tail an operator is looking for.
    #[test]
    fn rendering_puts_overflow_in_the_inf_bucket() {
        let h = Histogram::new();
        h.record_us(1_000);
        h.record_us(120_000_000);
        let mut out = String::new();
        h.render(&mut out, "fastllm_test", "model=\"m\"");

        assert!(out.contains("fastllm_test_bucket{model=\"m\",le=\"0.001000\"} 1"));
        assert!(out.contains("fastllm_test_bucket{model=\"m\",le=\"60.000000\"} 1"));
        assert!(out.contains("fastllm_test_bucket{model=\"m\",le=\"+Inf\"} 2"));
        assert!(out.contains("fastllm_test_count{model=\"m\"} 2"));
    }

    /// Durations are published in seconds because that is what
    /// `histogram_quantile` and every stock dashboard assume; a millisecond
    /// ladder silently reads as 1000x too slow.
    #[test]
    fn bounds_render_as_exact_decimal_seconds() {
        assert_eq!(seconds(100), "0.000100");
        assert_eq!(seconds(1_000), "0.001000");
        assert_eq!(seconds(2_500_000), "2.500000");
        assert_eq!(seconds(60_000_000), "60.000000");
    }

    #[test]
    fn an_empty_histogram_renders_zeroes_rather_than_nothing() {
        // A series that appears only after the first request makes a dashboard
        // read as "no data" instead of "no traffic", and the two need different
        // responses.
        let mut out = String::new();
        Histogram::new().render(&mut out, "fastllm_test", "");
        assert!(out.contains("fastllm_test_bucket{le=\"+Inf\"} 0"));
        assert!(out.contains("fastllm_test_count{} 0"));
    }

    #[test]
    fn concurrent_records_are_not_lost() {
        use std::sync::Arc;
        let h = Arc::new(Histogram::new());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let h = Arc::clone(&h);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        h.record_us(1_500);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(h.count(), 80_000);
        assert_eq!(h.sum_us(), 80_000 * 1_500);
    }
}
