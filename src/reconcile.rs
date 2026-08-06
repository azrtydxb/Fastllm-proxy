//! The data-plane side of P2's reconciliation loop: periodically reports
//! locally observed request/token counts (`crate::limiter::Limiter::drain_counts`)
//! to the control plane and applies the allowance it hands back
//! (`crate::limiter::Limiter::apply_allowances`).
//!
//! Structurally this mirrors `crate::usage`'s spawn/ticker/flush shape and
//! reuses the same `Upstream` client and bearer-token pattern -- see that
//! module's doc comment for the "own I/O on its own schedule, never on the
//! request path" rationale, which carries over unchanged. It cannot be the
//! *same* channel, though: usage reporting is fire-and-forget (`POST /usage`
//! returns nothing the proxy consumes), while reconciliation is a round
//! trip by design -- the whole point is the allowance the control plane
//! hands back in the response body, which nothing about `UsageReporter`'s
//! one-way queue has anywhere to put.
//!
//! Only spawned in `Http`-mode `--role proxy` (see `main.rs`). `File` mode
//! has no control plane to talk to, and `--role all` has nothing to
//! reconcile with -- see `crate::limiter`'s doc comment for why that mode
//! simply never needs this loop running at all, rather than needing it
//! specially suppressed.

use crate::limiter::{Allowance, Limiter, ObservedCounts};
use crate::upstream::Upstream;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize)]
struct ReportedCount {
    principal_id: u64,
    requests: u64,
    tokens: u64,
}

#[derive(Serialize)]
struct ReconcileRequestBody {
    /// Identifies this replica across reporting ticks so the control plane
    /// can tell "the same replica reporting again" from "a different
    /// replica" -- see `control::reconcile::ReconcileState`. Any string
    /// unique for the process's lifetime works; it is never parsed, only
    /// used as a map key.
    replica_id: String,
    counts: Vec<ReportedCount>,
}

#[derive(Deserialize)]
struct AllowanceWire {
    principal_id: u64,
    requests_share: f64,
    tokens_share: f64,
}

#[derive(Deserialize)]
struct ReconcileResponseBody {
    allowances: Vec<AllowanceWire>,
}

pub struct ReconcileConfig {
    /// Full `POST /limits/reconcile` URL.
    pub url: String,
    /// The proxy's own bootstrap token, same one presented to `/snapshot`
    /// and `/usage`.
    pub token: String,
    /// Unique to this process. `main.rs` generates one at startup.
    pub replica_id: String,
    pub interval: Duration,
}

/// Spawn the background reconciliation loop. Returns nothing: unlike
/// `usage::spawn`, nothing on the request path calls back into this --
/// `Limiter::check` already updates the counters this loop reads, and
/// applying an allowance is this loop's own job, not something a caller
/// hands events to.
pub fn spawn(cfg: ReconcileConfig, limiter: Arc<Limiter>, upstream: Arc<Upstream>) {
    tokio::spawn(run(cfg, limiter, upstream));
}

async fn run(cfg: ReconcileConfig, limiter: Arc<Limiter>, upstream: Arc<Upstream>) {
    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let counts = limiter.drain_counts();
        if counts.is_empty() {
            // Nothing rate-limited has been touched since the last tick;
            // nothing to report and nothing to ask for.
            continue;
        }
        match report(&cfg, &upstream, &counts).await {
            Ok(allowances) => limiter.apply_allowances(&allowances),
            Err(e) => {
                // Keep whatever shares were already in effect. Neither
                // resetting to "unlimited" nor collapsing to "zero" is
                // better than simply trying again next tick -- a missed
                // reconciliation is exactly the cost the design doc accepts
                // ("a limit can be exceeded by up to one reconciliation
                // window's worth of traffic"), not a reason to make the
                // failure mode worse in either direction.
                tracing::warn!(
                    error = %e,
                    "rate limit reconciliation failed; keeping the previous allowance"
                );
            }
        }
    }
}

async fn report(
    cfg: &ReconcileConfig,
    upstream: &Upstream,
    counts: &[ObservedCounts],
) -> anyhow::Result<Vec<Allowance>> {
    let body = serde_json::to_vec(&ReconcileRequestBody {
        replica_id: cfg.replica_id.clone(),
        counts: counts
            .iter()
            .map(|c| ReportedCount {
                principal_id: c.principal_id,
                requests: c.requests,
                tokens: c.tokens,
            })
            .collect(),
    })?;

    let req = Request::builder()
        .method("POST")
        .uri(&cfg.url)
        .header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", cfg.token),
        )
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))?;

    let resp = tokio::time::timeout(Duration::from_secs(10), upstream.request(req))
        .await
        .map_err(|_| anyhow::anyhow!("reconciliation request timed out"))??;

    if !resp.status().is_success() {
        anyhow::bail!("control plane returned {}", resp.status());
    }
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("reading reconciliation response body: {e}"))?
        .to_bytes();
    let parsed: ReconcileResponseBody = serde_json::from_slice(&bytes)?;
    Ok(parsed
        .allowances
        .into_iter()
        .map(|a| Allowance {
            principal_id: a.principal_id,
            requests_share: a.requests_share,
            tokens_share: a.tokens_share,
        })
        .collect())
}
