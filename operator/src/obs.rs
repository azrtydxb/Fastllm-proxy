//! The operator's own metrics and probes.
//!
//! A controller with no metrics is a controller you find out about from the
//! thing it was supposed to be managing. The three numbers that actually
//! answer "is it working" are here: how many reconciles have completed, how
//! many failed, and whether this replica is the leader — the last one being
//! the difference between "quiet because nothing changed" and "quiet because
//! this pod is a standby".
//!
//! Prometheus text format written by hand, for the same reason the proxy does
//! it: this is four counters and a scrape, not a use case for a client
//! library and its registry.

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::warn;

#[derive(Default)]
pub struct Metrics {
    pub reconciles: AtomicU64,
    pub errors: AtomicU64,
    pub resources: AtomicU64,
    pub leader: AtomicBool,
    /// Set once the process has a Kubernetes client and has seen the CRD.
    /// `/readyz` is this; `/healthz` is "the process is alive", and they are
    /// deliberately different — a standby replica is healthy and not ready to
    /// be the one doing the work.
    pub started: AtomicBool,
}

impl Metrics {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str("# HELP fastllm_operator_reconcile_total Reconciles that completed.\n");
        out.push_str("# TYPE fastllm_operator_reconcile_total counter\n");
        out.push_str(&format!(
            "fastllm_operator_reconcile_total {}\n",
            self.reconciles.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP fastllm_operator_reconcile_errors_total Reconciles that failed.\n");
        out.push_str("# TYPE fastllm_operator_reconcile_errors_total counter\n");
        out.push_str(&format!(
            "fastllm_operator_reconcile_errors_total {}\n",
            self.errors.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP fastllm_operator_managed_resources FastllmProxy resources seen.\n");
        out.push_str("# TYPE fastllm_operator_managed_resources gauge\n");
        out.push_str(&format!(
            "fastllm_operator_managed_resources {}\n",
            self.resources.load(Ordering::Relaxed)
        ));
        out.push_str("# HELP fastllm_operator_leader 1 if this replica holds the leader lease.\n");
        out.push_str("# TYPE fastllm_operator_leader gauge\n");
        out.push_str(&format!(
            "fastllm_operator_leader {}\n",
            u8::from(self.leader.load(Ordering::Relaxed))
        ));
        out
    }
}

/// Serve `/metrics`, `/healthz` and `/readyz` until the process ends.
///
/// Failure to bind is logged and otherwise ignored: an operator that refused
/// to reconcile because its metrics port was taken would be trading the job
/// for the instrumentation of the job.
pub fn spawn(metrics: Arc<Metrics>, addr: SocketAddr) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, %addr, "metrics listener did not start; continuing without it");
                return;
            }
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let metrics = Arc::clone(&metrics);
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let metrics = Arc::clone(&metrics);
                    async move { Ok::<_, std::convert::Infallible>(route(&req, &metrics)) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

fn route(req: &Request<Incoming>, metrics: &Metrics) -> Response<Full<Bytes>> {
    let body = |status: StatusCode, body: String| {
        Response::builder()
            .status(status)
            .body(Full::new(Bytes::from(body)))
            .expect("static response")
    };
    match req.uri().path() {
        "/metrics" => body(StatusCode::OK, metrics.render()),
        "/healthz" => body(StatusCode::OK, "ok\n".into()),
        "/readyz" => {
            if metrics.started.load(Ordering::Relaxed) {
                body(StatusCode::OK, "ok\n".into())
            } else {
                body(StatusCode::SERVICE_UNAVAILABLE, "starting\n".into())
            }
        }
        _ => body(StatusCode::NOT_FOUND, "not found\n".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_counter_is_present_before_anything_has_happened() {
        // A series that only appears after the first reconcile reads as "no
        // data" on a dashboard, which is indistinguishable from "operator is
        // down" — the one thing the dashboard exists to tell apart.
        let rendered = Metrics::default().render();
        for name in [
            "fastllm_operator_reconcile_total 0",
            "fastllm_operator_reconcile_errors_total 0",
            "fastllm_operator_managed_resources 0",
            "fastllm_operator_leader 0",
        ] {
            assert!(rendered.contains(name), "missing {name}:\n{rendered}");
        }
    }

    #[test]
    fn leadership_renders_as_one() {
        let m = Metrics::default();
        m.leader.store(true, Ordering::Relaxed);
        assert!(m.render().contains("fastllm_operator_leader 1"));
    }
}
