//! Shared process state.

use arc_swap::ArcSwap;
use bytes::Bytes;
use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::registry::{Interner, Registry};
use crate::router::Router;

pub type HttpClient = Client<HttpConnector, Full<Bytes>>;

pub struct AppState {
    /// Swapped wholesale on reload; readers never block.
    pub registry: ArcSwap<Registry>,
    pub router: Router,
    pub client: HttpClient,
    pub interner: Interner,
    pub config_path: PathBuf,

    pub master_key: Option<String>,
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
