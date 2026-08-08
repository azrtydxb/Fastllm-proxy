//! The data-plane side of the Snapshot protocol's reverse channel: batched,
//! fire-and-forget usage reporting to the control plane's `POST /usage`.
//!
//! Nothing calls [`UsageReporter::record`] yet — P2 wires real token counting
//! into the request path once streaming responses are parsed for a trailing
//! `usage` object (see the design doc's P3 section). This module exists now,
//! in P0, specifically so that plumbing does not have to reshape the wire
//! protocol later: the batch shape, the queue, and the drop-on-full policy
//! are all fixed today. `UsageReporter::record` is the entire surface that
//! future caller needs.
//!
//! Design constraint carried over from the request path itself: recording a
//! usage event must never be able to block, retry-storm, or grow memory
//! without bound, no matter how slow or unreachable the control plane is.
//! `record` is therefore a non-blocking `try_send` into a bounded channel; a
//! full queue means events are dropped, and the drop count is exposed on
//! `/metrics` so the loss is visible rather than silent — silently discarding
//! data used for billing would be worse than admitting it happened.

use crate::upstream::Upstream;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// One request's token accounting. The wire shape `POST /usage` accepts, and
/// the shape this module hands a future caller — one type shared with the
/// control plane's decode side (`control::api::post_usage`), so the two ends
/// of the wire cannot drift apart the way two independent struct
/// definitions could.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub principal_id: u64,
    /// Model *name*, not an internal database id: the data plane only ever
    /// knows the model the way the snapshot named it
    /// (`snapshot::ModelDef::name`). The control plane resolves this to
    /// `models.id` at ingest time and drops the row if it does not resolve.
    pub model: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub at: chrono::DateTime<chrono::Utc>,
    /// Whole-request wall time. `None` only when the event was produced
    /// somewhere that does not time (the reconciler, a test).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// Time to first token, for a streamed response. `None` for a buffered
    /// one, where it would be a copy of `duration_ms` rather than a second
    /// measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u32>,
    /// The upstream's status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// What the client asked for, when it differs from `model` — a virtual
    /// model, or the head of a chain that failed over. `model` is always the
    /// one that actually answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
    /// What the provider says it charged, in micro-units.
    ///
    /// Authoritative where present: it is the amount actually billed, it
    /// already accounts for cache discounts and for a routed alias serving a
    /// different model per request, and it does not go stale when a provider
    /// changes its prices. The control plane falls back to the configured
    /// price only when this is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
}

#[derive(Serialize)]
struct UsageBatch<'a> {
    events: &'a [UsageEvent],
}

/// Settings for the background flush task. Kept modest: usage rows are small
/// and this reports every few seconds, not a firehose — P2's real caller can
/// revisit these once there is real traffic to tune against.
pub struct ReporterConfig {
    /// Full `POST /usage` URL, e.g. `https://control:4001/usage`.
    pub url: String,
    /// The proxy's own bootstrap token — the same one `HttpSource` presents
    /// to `GET /snapshot`.
    pub token: String,
    /// Bound on events awaiting their next flush. Beyond this, `record`
    /// drops rather than grows the queue further.
    pub queue_capacity: usize,
    /// Flush early once a batch reaches this size, rather than waiting for
    /// `flush_interval`.
    pub batch_max: usize,
    pub flush_interval: Duration,
}

/// Handed to whatever wires up token counting in P2. Cloning shares the same
/// queue and drop counter — every clone reports into the same flush task.
#[derive(Clone)]
pub struct UsageReporter {
    tx: mpsc::Sender<UsageEvent>,
    dropped: Arc<AtomicU64>,
}

impl UsageReporter {
    /// Enqueue one event for the next batch flush. Never blocks: a full
    /// queue means the control plane (or the network to it) is not keeping
    /// up, and the design's answer to that is to drop the event, not to
    /// apply backpressure to inference. See the module doc comment.
    pub fn record(&self, event: UsageEvent) {
        if self.tx.try_send(event).is_err() {
            // Both a full queue and a dead receiver (the flush task panicked,
            // or this is a `disabled()` reporter with no receiver at all)
            // land here; either way there is nothing to do but count it.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Events enqueued and then discarded rather than delivered, because the
    /// queue was full when `record` was called. Exposed on `/metrics`
    /// (`fastllm_usage_reports_dropped_total`) so a control plane that
    /// cannot keep up is visible instead of silently losing billing data.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// A reporter with nowhere to send, for deployments with no control
    /// plane to report to (`File` mode) or where P2's wiring has not decided
    /// what `--role all` should do yet (it *could* write straight to
    /// Postgres rather than loop an HTTP request back to itself — a decision
    /// this module deliberately does not make). `record` still works, and
    /// still drops under backpressure exactly as the real thing does, since
    /// the channel has no consumer — so callers get one type and one
    /// non-blocking method regardless of mode, never an `Option` to branch
    /// on.
    pub fn disabled() -> Self {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        Self {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    #[cfg(test)]
    fn with_capacity_for_test(cap: usize) -> (Self, mpsc::Receiver<UsageEvent>) {
        let (tx, rx) = mpsc::channel(cap);
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }
}

/// Spawn the background flush task and return the handle a future P2 caller
/// records through. The task owns the receiving half of the channel and does
/// all its I/O off to the side, on its own schedule — nothing on the request
/// path ever waits on it.
pub fn spawn(cfg: ReporterConfig, upstream: Arc<Upstream>) -> UsageReporter {
    let (tx, rx) = mpsc::channel(cfg.queue_capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    tokio::spawn(run(cfg, upstream, rx));
    UsageReporter { tx, dropped }
}

async fn run(cfg: ReporterConfig, upstream: Arc<Upstream>, mut rx: mpsc::Receiver<UsageEvent>) {
    let mut batch = Vec::with_capacity(cfg.batch_max);
    let mut ticker = tokio::time::interval(cfg.flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(event) => {
                        batch.push(event);
                        if batch.len() >= cfg.batch_max {
                            flush(&cfg, &upstream, &mut batch).await;
                        }
                    }
                    // The sender (and every `UsageReporter` clone) is gone:
                    // the process is shutting down. Flush what is left, then
                    // exit rather than spin on a closed channel.
                    None => {
                        flush(&cfg, &upstream, &mut batch).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                flush(&cfg, &upstream, &mut batch).await;
            }
        }
    }
}

/// One best-effort delivery attempt. Fire-and-forget per the design: a
/// failure is logged and the batch is dropped, never retried — retrying
/// would either block later flushes behind a dead control plane or need its
/// own unbounded buffer, both of which reintroduce the coupling this module
/// exists to avoid. "Dropping usage rather than blocking a request is
/// deliberate — billing accuracy is not worth failing inference."
async fn flush(cfg: &ReporterConfig, upstream: &Upstream, batch: &mut Vec<UsageEvent>) {
    if batch.is_empty() {
        return;
    }
    let count = batch.len();
    let body = match serde_json::to_vec(&UsageBatch { events: batch }) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, count, "could not encode usage batch; dropping it");
            batch.clear();
            return;
        }
    };
    batch.clear();

    let req = match Request::builder()
        .method("POST")
        .uri(&cfg.url)
        .header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", cfg.token),
        )
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "could not build usage report request; dropping batch");
            return;
        }
    };

    match tokio::time::timeout(Duration::from_secs(10), upstream.request(req)).await {
        Ok(Ok(resp)) if resp.status().is_success() => {
            let _ = resp.into_body().collect().await;
        }
        Ok(Ok(resp)) => {
            let status = resp.status();
            let _ = resp.into_body().collect().await;
            tracing::warn!(%status, count, "control plane rejected usage batch; dropped");
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, count, "sending usage batch failed; dropped");
        }
        Err(_) => {
            tracing::warn!(count, "sending usage batch timed out; dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(principal_id: u64) -> UsageEvent {
        UsageEvent {
            principal_id,
            model: "m".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
            at: chrono::Utc::now(),
            duration_ms: None,
            ttft_ms: None,
            status: None,
            requested_model: None,
            cost_micros: None,
        }
    }

    /// The property the request path depends on: `record` past capacity
    /// drops the event and counts it, rather than waiting for room —
    /// `try_send` never blocks by construction, so this is really a proof
    /// that `record` is built on `try_send` and not, say, `send().await`.
    #[test]
    fn recording_past_capacity_drops_and_counts_rather_than_blocking() {
        let (reporter, mut rx) = UsageReporter::with_capacity_for_test(2);
        reporter.record(event(1));
        reporter.record(event(2));
        reporter.record(event(3));

        assert_eq!(
            reporter.dropped(),
            1,
            "one of three sends must have found the queue full"
        );
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(
            rx.try_recv().is_err(),
            "only the two that fit should have been enqueued"
        );
    }

    #[test]
    fn the_drop_count_is_observable_and_keeps_counting() {
        let (reporter, _rx) = UsageReporter::with_capacity_for_test(1);
        reporter.record(event(1));
        assert_eq!(reporter.dropped(), 0);
        reporter.record(event(2));
        reporter.record(event(3));
        assert_eq!(reporter.dropped(), 2);
    }

    /// `disabled()` exists for `File` mode and any other case with no
    /// control plane to report to. It must not panic or silently succeed —
    /// every `record` on it is visible as a drop, same as a real reporter
    /// whose queue is permanently full.
    #[test]
    fn a_disabled_reporter_drops_everything_and_counts_it() {
        let reporter = UsageReporter::disabled();
        reporter.record(event(1));
        reporter.record(event(2));
        assert_eq!(reporter.dropped(), 2);
    }
}
