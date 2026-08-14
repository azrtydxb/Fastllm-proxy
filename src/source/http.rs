//! Polls a control plane, and keeps serving when it is not there.
//!
//! The disk cache is the whole point: a proxy that cannot reach its control
//! plane keeps serving on the last policy it saw and merely stops learning
//! about changes. Losing the control plane must not lose inference.

use crate::snapshot::Snapshot;
use crate::source::SnapshotSource;
use crate::upstream::Upstream;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct HttpSource {
    url: String,
    token: String,
    cache_path: PathBuf,
    upstream: Arc<Upstream>,
}

impl HttpSource {
    /// `upstream` is not built here: this crate owns exactly one pooled HTTP
    /// client (see `upstream::Upstream`'s doc comment for why), and every
    /// caller of a control plane or a backend shares it rather than each
    /// standing up its own connection pool.
    pub fn new(url: String, token: String, cache_path: PathBuf, upstream: Arc<Upstream>) -> Self {
        Self {
            url,
            token,
            cache_path,
            upstream,
        }
    }

    pub fn write_cache(&self, snap: &Snapshot) -> anyhow::Result<()> {
        if let Some(dir) = self.cache_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write and rename, so a crash mid-write cannot leave a torn file that
        // the next start would reject.
        let tmp = self.cache_path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&snap.to_wire())?)?;
        std::fs::rename(&tmp, &self.cache_path)?;
        Ok(())
    }

    /// Last-known-good, or `None` for absent, unreadable or corrupt.
    pub fn load_cached(&self) -> Option<Snapshot> {
        let bytes = std::fs::read(&self.cache_path).ok()?;
        let wire = serde_json::from_slice(&bytes).ok()?;
        Some(Snapshot::from_wire(wire))
    }
}

impl SnapshotSource for HttpSource {
    async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
        let mut builder = Request::builder().method("GET").uri(&self.url).header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", self.token),
        );
        if let Some(v) = have {
            builder = builder.header("if-none-match", format!("\"{v}\""));
        }
        let req = builder.body(Full::new(Bytes::new()))?;

        let resp = tokio::time::timeout(Duration::from_secs(10), self.upstream.request(req))
            .await
            .map_err(|_| anyhow::anyhow!("fetching {} timed out", self.url))??;

        if resp.status() == StatusCode::NOT_MODIFIED {
            return Ok(None);
        }
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!(e))?
            .to_bytes();
        if !status.is_success() {
            anyhow::bail!("control plane returned {status} fetching {}", self.url);
        }

        let wire = serde_json::from_slice(&body)?;
        let snap = Snapshot::from_wire(wire);
        // Cache failures are logged, never fatal: a read-only filesystem
        // should degrade the outage story, not stop the proxy.
        if let Err(e) = self.write_cache(&snap) {
            tracing::warn!(error = %e, "could not write the snapshot cache");
        }
        Ok(Some(snap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{Principal, Snapshot};

    fn a_snapshot() -> Snapshot {
        Snapshot::for_test(
            vec![("sk-x".into(), 1, None, false)],
            vec![Principal {
                id: 1,
                name: "p".into(),
                allowed_models: ["m".to_string()].into_iter().collect(),
                allow_all: false,
                allowed_mcp: Default::default(),
                allow_all_mcp: false,
                roles: Default::default(),
                limits: None,
                budget: None,
            }],
            vec![],
        )
    }

    /// No network call is made in these tests, so the pool never has to dial
    /// anything real; only `Upstream::new`'s bookkeeping is exercised.
    fn test_upstream() -> Arc<Upstream> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        Arc::new(Upstream::new(
            crate::upstream::Config {
                max_idle_per_host: 1,
                idle_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_secs(1),
            },
            tls,
        ))
    }

    #[test]
    fn a_snapshot_survives_a_round_trip_through_the_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        let src = HttpSource::new(
            "http://unused".into(),
            "t".into(),
            path.clone(),
            test_upstream(),
        );
        src.write_cache(&a_snapshot()).unwrap();

        let loaded = src.load_cached().expect("cache should load");
        let p = loaded
            .authenticate("sk-x", std::time::SystemTime::now())
            .unwrap();
        assert_eq!(p.name, "p");
        assert!(p.may_invoke("m"));
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let src = HttpSource::new(
            "http://unused".into(),
            "t".into(),
            dir.path().join("nope.json"),
            test_upstream(),
        );
        assert!(src.load_cached().is_none());
    }

    #[test]
    fn a_corrupt_cache_is_ignored_rather_than_fatal() {
        // A truncated write must not stop the proxy from starting.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let src = HttpSource::new("http://unused".into(), "t".into(), path, test_upstream());
        assert!(src.load_cached().is_none());
    }
}
