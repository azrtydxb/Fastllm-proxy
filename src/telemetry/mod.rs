//! Telemetry that the request path can afford.
//!
//! The constraint everything here answers to: the proxy's own per-request work
//! measures well under a microsecond against roughly 38µs of core time
//! (`bench/micro`, `docs/performance.md`). Telemetry that cost even a
//! microsecond would double it. So the rules are the same throughout —
//!
//! - **No allocation and no formatting while serving.** Label strings are
//!   resolved when the snapshot is built, not per request; the only place a
//!   string is built is `/metrics`, at scrape time.
//! - **No locks.** Every counter is a relaxed atomic add.
//! - **Nothing unbounded.** Prometheus labels stay to values fixed by
//!   configuration (model, backend, outcome). Per-caller and per-request
//!   detail goes to the usage channel instead, which is batched off the
//!   request path and lands in Postgres where cardinality is somebody else's
//!   problem.
//!
//! What that buys is measured rather than asserted: `bench/micro` records the
//! cost of each instrument, and `docs/performance.md` carries the numbers.

pub mod histogram;
pub mod metrics;

#[cfg(feature = "otel")]
pub mod tracing_otel;

pub use histogram::Histogram;
pub use metrics::{ModelMetrics, Outcome, Rejection, RequestTiming, Telemetry};
