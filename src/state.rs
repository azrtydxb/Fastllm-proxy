//! Shared process state.

use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::registry::{Interner, Registry};
use crate::router::Router;
use crate::snapshot::Snapshot;

/// Speaks both schemes: cluster-local vLLM nodes are plain HTTP, but a config
/// may equally point at a TLS-terminated or hosted endpoint.
pub type HttpClient = crate::upstream::Upstream;

pub struct AppState {
    /// Swapped wholesale on reload; readers never block.
    pub registry: ArcSwap<Registry>,
    pub router: Router,
    pub client: HttpClient,
    pub interner: Interner,
    pub config_path: PathBuf,

    /// Set once from `FileSource` at startup and only ever `.load()`ed on the
    /// request path since. Unlike `registry` above, nothing currently calls
    /// `.store()` on this: SIGHUP and the config-poll watcher still only
    /// rebuild the routing `Registry`, so key and grant changes need a
    /// process restart today. Wiring a snapshot source into that same reload
    /// path is what will make them live.
    pub snapshot: ArcSwap<Snapshot>,
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
    /// Rebuild the registry from disk and swap it in.
    ///
    /// Live backends carry over, so a reload does not reset health or lose
    /// in-flight accounting for connections still streaming.
    pub fn reload(&self) -> anyhow::Result<usize> {
        let cfg = crate::config::FileConfig::load(&self.config_path)?;
        let current = self.registry.load();
        let next = Registry::build(&cfg, &self.interner, Some(&current))?;
        let count = next.backends().len();
        self.registry.store(Arc::new(next));
        Ok(count)
    }
}
