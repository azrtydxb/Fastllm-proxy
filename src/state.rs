//! Shared process state.

use anyhow::Context;
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::registry::{Interner, Registry};
use crate::router::Router;
use crate::snapshot::Snapshot;
use crate::source::file::FileSource;
use crate::source::{SnapshotSource, WithLegacyMasterKey};

/// Speaks both schemes: cluster-local vLLM nodes are plain HTTP, but a config
/// may equally point at a TLS-terminated or hosted endpoint.
///
/// `Arc`-wrapped so the one pooled client this crate ever builds (see
/// `upstream::Upstream`'s doc comment) can also be handed to an `HttpSource`
/// without standing up a second pool.
pub type HttpClient = Arc<crate::upstream::Upstream>;

pub struct AppState {
    /// Swapped wholesale on reload; readers never block.
    pub registry: ArcSwap<Registry>,
    pub router: Router,
    pub client: HttpClient,
    pub interner: Interner,
    pub config_path: PathBuf,

    /// Deprecated `--master-key`/`general_settings.master_key`, carried here
    /// so `reload()` (SIGHUP) can re-merge it into every fetch — see
    /// `source::WithLegacyMasterKey` for why that merge cannot just happen
    /// once at startup.
    pub legacy_master_key: Option<String>,

    /// `Arc`-wrapped so `--role all` can hand the *same* cell to the control
    /// plane's admin API (`control::api::serve`'s `cache` argument): a key
    /// created or revoked over the admin API is visible to the proxy's next
    /// request with no polling and no HTTP round trip back to itself.
    ///
    /// Kept current in every role: `--role all` writes it directly from the
    /// admin API, `File` and `Http` modes are kept current by
    /// `source::spawn_poller`, and `reload()` below re-fetches it on SIGHUP.
    /// Every one of those paths also calls `rebuild_registry_from_snapshot`,
    /// so `registry` above is never allowed to drift from what this says is
    /// authorised to run.
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub max_body_bytes: usize,
    pub max_retries: usize,
    /// Bounds time-to-first-byte only. Generation itself is unbounded — a long
    /// completion is not a hung upstream.
    pub upstream_headers_timeout: Duration,
    pub unhealthy_after: u32,

    pub started: Instant,
    pub requests_ok: AtomicU64,
    pub requests_failed: AtomicU64,
}

impl AppState {
    /// Rebuild the routing `Registry` from the snapshot already stored in
    /// `self.snapshot`, and swap it in.
    ///
    /// Live backends carry over, so a reload does not reset health or lose
    /// in-flight accounting for connections still streaming. Called after
    /// every snapshot change, from whichever of the three paths on
    /// `snapshot`'s doc comment produced it.
    pub fn rebuild_registry_from_snapshot(&self) -> anyhow::Result<usize> {
        let snap = self.snapshot.load();
        let current = self.registry.load();
        let next = Registry::build_from_snapshot(&snap, &self.interner, Some(&current))?;
        let count = next.backends().len();
        self.registry.store(Arc::new(next));
        Ok(count)
    }

    /// SIGHUP's handler in `File` mode: re-read the config file right now
    /// rather than waiting for the next poll tick, and apply it to both the
    /// snapshot and the registry built from it.
    ///
    /// Only meaningful when `config_path` is the actual source of truth,
    /// i.e. `File` mode — `main.rs` only wires the SIGHUP listener up in that
    /// case. `Http` and `all` deployments are kept current by
    /// `source::spawn_poller` and by the admin API's write-time refresh
    /// respectively, and re-parsing `config_path` there would be reloading
    /// the wrong source of truth.
    pub async fn reload(&self) -> anyhow::Result<usize> {
        let source = WithLegacyMasterKey::new(
            FileSource::new(self.config_path.clone()),
            self.legacy_master_key.clone(),
        );
        let snap = source
            .fetch(None)
            .await?
            .context("config produced no snapshot on reload")?;
        self.snapshot.store(Arc::new(snap));
        self.rebuild_registry_from_snapshot()
    }
}
