//! Proxies telling the control plane what they can see.
//!
//! # Why the control plane cannot work this out itself
//!
//! Backend health lives in the data plane: each proxy probes its own backends
//! and keeps its own in-flight counts. The control plane has never seen any of
//! it — it publishes a snapshot and hears back only about usage. So a
//! management UI asking "is this backend up, and how loaded is it" had nowhere
//! to ask, and the only honest answer was to scrape every proxy's `/metrics`
//! individually and hope the UI knew where they all were.
//!
//! This is the reverse channel that already exists for usage, carrying one more
//! kind of fact over the same `--proxy-token`.
//!
//! # Per replica, deliberately
//!
//! A report says *this proxy* believes *this backend* is up, with *this many*
//! requests in flight. It is not merged into a fleet-wide truth, because the
//! interesting failures are exactly the ones where replicas disagree — one
//! proxy that cannot reach a backend the others can is a network partition,
//! and averaging it away hides the only symptom.
//!
//! # Not durable
//!
//! Held in memory by the control plane and lost on restart. Health is a
//! statement about *now*; a row in Postgres saying a backend was up two hours
//! ago is not health, it is history nobody asked for. A proxy that stops
//! reporting simply ages out.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::upstream::Upstream;

/// What one proxy can see, at one moment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    /// Which proxy. Its hostname, which in Kubernetes is the pod name — the
    /// thing an operator would `kubectl logs` next.
    pub replica: String,
    /// The snapshot this proxy is serving, so a fleet-wide `max - min` shows a
    /// replica stuck on an old configuration without scraping each one.
    pub snapshot_version: u64,
    pub uptime_seconds: u64,
    pub backends: Vec<BackendHealth>,
    /// Counters that are per process by construction, carried so a fleet view
    /// can show the *spread* rather than a sum that would mean nothing. The
    /// response cache is not shared between replicas, and a worker restarted
    /// five minutes ago has a cold one — averaging its hit rate into a fleet
    /// figure reports a problem where there is none.
    #[serde(default)]
    pub process: ProcessCounters,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessCounters {
    pub requests_ok: u64,
    pub requests_failed: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub cache_entries: usize,
    pub cache_bytes: u64,
    /// Usage events this replica dropped because the control plane was not
    /// keeping up. Deliberately surfaced: the design's stated trade is to drop
    /// usage rather than block inference, and a silent drop makes billing
    /// quietly wrong instead of visibly incomplete.
    pub usage_dropped: u64,
    /// Refusals this replica made *before* it could attribute a request to
    /// anyone, so they have no `usage_events` row and never will.
    ///
    /// Only the kinds that are genuinely missing from that table are here.
    /// 403/429/402 and the unreachable-chain 502 are recorded there as
    /// `refusal` rows, and reporting them again would double-count every one
    /// of them in any total built from both sources.
    ///
    /// Cumulative per process, like every other counter here; the control
    /// plane turns them into deltas by comparing against this replica's
    /// previous report.
    #[serde(default)]
    pub rejected_unauthenticated: u64,
    #[serde(default)]
    pub rejected_model_not_found: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendHealth {
    pub api_base: String,
    pub model: String,
    pub healthy: bool,
    pub inflight: usize,
    pub requests_total: u64,
    pub errors_total: u64,
}

/// Sends a report every `interval`.
///
/// A plain interval rather than on-change: health is a level, not an event, and
/// a UI polling the control plane wants the current value rather than a
/// reconstruction from a stream of transitions it may have missed.
pub struct Reporter {
    tx: mpsc::Sender<HealthReport>,
}

impl Reporter {
    /// Queue a report, dropping it if the channel is full.
    ///
    /// Same rule as usage: a control plane that is not keeping up must not
    /// apply backpressure to a proxy. A dropped report costs one stale tile in
    /// a UI until the next one, which is a much better failure than a proxy
    /// blocking on it.
    pub fn send(&self, report: HealthReport) {
        let _ = self.tx.try_send(report);
    }
}

pub struct Config {
    /// `POST` target, e.g. `https://control:4001/health-report`.
    pub url: String,
    pub token: String,
    pub interval: Duration,
}

pub fn spawn(cfg: Config, upstream: Arc<Upstream>) -> Reporter {
    // Depth 1: a backlog of health reports is worthless, because only the
    // newest says anything true. Queuing more would deliver a UI a sequence of
    // stale answers rather than one current one.
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(run(cfg, upstream, rx));
    Reporter { tx }
}

async fn run(cfg: Config, upstream: Arc<Upstream>, mut rx: mpsc::Receiver<HealthReport>) {
    while let Some(report) = rx.recv().await {
        if let Err(e) = post(&cfg, &upstream, &report).await {
            // A control plane that is down must not make a proxy noisy: this is
            // the same "keep serving, stop learning" posture the snapshot
            // poller takes, and inference is unaffected either way.
            tracing::debug!(error = %e, "could not deliver a health report");
        }
    }
}

async fn post(cfg: &Config, upstream: &Upstream, report: &HealthReport) -> anyhow::Result<()> {
    use http_body_util::BodyExt as _;

    let body = serde_json::to_vec(report)?;
    let req = hyper::Request::builder()
        .method("POST")
        .uri(&cfg.url)
        .header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", cfg.token),
        )
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))?;

    let resp = tokio::time::timeout(Duration::from_secs(5), upstream.request(req))
        .await
        .map_err(|_| anyhow::anyhow!("health report timed out"))??;
    let status = resp.status();
    // Drained rather than dropped, so the connection returns to the pool
    // instead of being torn down once a second.
    let _ = resp.into_body().collect().await;
    if !status.is_success() {
        anyhow::bail!("control plane answered {status}");
    }
    Ok(())
}

/// What the control plane keeps: the latest report from each replica, and when
/// it arrived.
#[cfg(feature = "control")]
pub mod store {
    use super::HealthReport;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    pub struct Fleet {
        reports: parking_lot::Mutex<HashMap<String, (HealthReport, Instant)>>,
        /// A replica that has not reported for this long is dropped rather than
        /// shown as stale. A scaled-down pod would otherwise sit in a UI
        /// forever, and "up 40 minutes ago" is not health.
        ttl: Duration,
    }

    impl Fleet {
        pub fn new(ttl: Duration) -> Self {
            Self {
                reports: parking_lot::Mutex::new(HashMap::new()),
                ttl,
            }
        }

        /// Record a report, returning the one it replaced.
        ///
        /// The previous report is what turns cumulative counters into
        /// deltas, and this is the only place both are in hand at once —
        /// returning it here avoids a second lookup that could race with
        /// another replica's report landing in between.
        pub fn record(&self, report: HealthReport, now: Instant) -> Option<HealthReport> {
            self.reports
                .lock()
                .insert(report.replica.clone(), (report, now))
                .map(|(previous, _)| previous)
        }

        /// Every replica heard from recently, newest first by replica name.
        ///
        /// Expiry happens here rather than on a timer: the only thing that
        /// reliably visits a stale entry is somebody asking for it.
        pub fn current(&self, now: Instant) -> Vec<HealthReport> {
            let mut reports = self.reports.lock();
            reports.retain(|_, (_, at)| now.duration_since(*at) < self.ttl);
            let mut out: Vec<HealthReport> = reports.values().map(|(r, _)| r.clone()).collect();
            out.sort_by(|a, b| a.replica.cmp(&b.replica));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(replica: &str, healthy: bool, version: u64) -> HealthReport {
        HealthReport {
            replica: replica.into(),
            snapshot_version: version,
            uptime_seconds: 1,
            process: Default::default(),
            backends: vec![BackendHealth {
                api_base: "http://a".into(),
                model: "m".into(),
                healthy,
                inflight: 2,
                requests_total: 10,
                errors_total: 0,
            }],
        }
    }

    /// Reports are kept per replica and not merged. The interesting failures
    /// are the ones where replicas disagree — one proxy that cannot reach a
    /// backend the others can is a partition, and averaging hides the only
    /// symptom there is.
    #[test]
    fn replicas_are_reported_separately_including_when_they_disagree() {
        use std::time::Instant;
        let fleet = store::Fleet::new(Duration::from_secs(30));
        let now = Instant::now();
        fleet.record(report("proxy-a", true, 100), now);
        fleet.record(report("proxy-b", false, 100), now);

        let current = fleet.current(now);
        assert_eq!(current.len(), 2);
        assert!(current[0].backends[0].healthy);
        assert!(!current[1].backends[0].healthy);
    }

    #[test]
    fn a_newer_report_replaces_the_previous_one_from_that_replica() {
        use std::time::Instant;
        let fleet = store::Fleet::new(Duration::from_secs(30));
        let now = Instant::now();
        fleet.record(report("proxy-a", true, 100), now);
        fleet.record(report("proxy-a", false, 200), now);

        let current = fleet.current(now);
        assert_eq!(current.len(), 1, "one entry per replica, not a history");
        assert_eq!(current[0].snapshot_version, 200);
    }

    /// A scaled-down pod must leave, not linger. "Up, 40 minutes ago" is not
    /// health.
    #[test]
    fn a_replica_that_stops_reporting_ages_out() {
        use std::time::Instant;
        let fleet = store::Fleet::new(Duration::from_secs(30));
        let now = Instant::now();
        fleet.record(report("gone", true, 1), now);
        assert_eq!(fleet.current(now).len(), 1);
        assert!(fleet.current(now + Duration::from_secs(31)).is_empty());
    }

    /// The fleet-wide question a UI asks: is everyone on the same snapshot?
    #[test]
    fn a_replica_stuck_on_an_old_snapshot_is_visible_without_scraping_each_one() {
        use std::time::Instant;
        let fleet = store::Fleet::new(Duration::from_secs(30));
        let now = Instant::now();
        fleet.record(report("current", true, 200), now);
        fleet.record(report("stuck", true, 100), now);

        let versions: Vec<u64> = fleet
            .current(now)
            .iter()
            .map(|r| r.snapshot_version)
            .collect();
        assert_eq!(
            versions.iter().max().unwrap() - versions.iter().min().unwrap(),
            100
        );
    }
}
