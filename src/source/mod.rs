//! Where the data plane gets its policy.
//!
//! Three implementations, one trait, and forwarding cannot tell them apart:
//! `File` for a proxy with no control plane at all, `Local` for the
//! single-process role, `Http` for a proxy against a control plane. This
//! module also owns the polling loop (`spawn_poller`) that every role but
//! `all` uses to keep its snapshot — and the routing `Registry` built from it
//! — current without a restart.

pub mod file;
pub mod http;

use crate::snapshot::Snapshot;
use crate::state::AppState;
use arc_swap::ArcSwap;
use std::sync::Arc;
use std::time::Duration;

pub trait SnapshotSource: Send + Sync {
    /// Return a snapshot only if it is newer than `have`.
    ///
    /// `Ok(None)` means unchanged, which is the common case on every poll and
    /// must stay cheap.
    fn fetch(
        &self,
        have: Option<u64>,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<Snapshot>>> + Send;
}

/// Reapplies `--master-key`/`general_settings.master_key` to every snapshot an
/// inner source produces.
///
/// The legacy key is not part of the YAML schema `FileSource` parses (nor of
/// any control-plane snapshot) — it is bolted on at the CLI layer. Doing that
/// merge here, once, means every fetch (the initial load, a SIGHUP reload,
/// and every poll tick) carries the key, instead of only the very first
/// snapshot the process ever built. Without this a `File`-mode reload would
/// silently drop a deployment's only credential the moment the underlying
/// config changed.
pub struct WithLegacyMasterKey<S> {
    inner: S,
    key: Option<String>,
}

impl<S> WithLegacyMasterKey<S> {
    pub fn new(inner: S, key: Option<String>) -> Self {
        Self { inner, key }
    }
}

impl<S: SnapshotSource> SnapshotSource for WithLegacyMasterKey<S> {
    async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
        let Some(mut snap) = self.inner.fetch(have).await? else {
            return Ok(None);
        };
        if let Some(key) = &self.key {
            snap.add_legacy_master_key(key);
        }
        Ok(Some(snap))
    }
}

/// One poll. Swaps only when the source reports a new version, so a steady
/// state costs one HTTP request with an ETag and no allocation.
pub async fn poll_once(
    source: &impl SnapshotSource,
    cell: &ArcSwap<Snapshot>,
) -> anyhow::Result<()> {
    let have = Some(cell.load().version).filter(|v| *v != 0);
    if let Some(next) = source.fetch(have).await? {
        cell.store(Arc::new(next));
    }
    Ok(())
}

/// Keep `state.snapshot` current from `source`, rebuilding `state.registry`
/// from it whenever it actually changes.
///
/// This is the fix for the gap left by Task 3: a `Registry` reload and a
/// `Snapshot` reload used to be two different, unconnected code paths, so
/// only the routing table ever lived-reloaded and key/grant edits needed a
/// restart. Every fetch that returns a new snapshot goes straight into
/// `AppState::apply_snapshot`, which is the *only* place that may write
/// `state.snapshot` and always rebuilds `state.registry` from it in the same
/// call — so, unlike an earlier version of this function, there is no
/// separate "did it change, then rebuild" step here for a future edit to
/// forget. A `File`-mode ConfigMap edit takes effect the same way a
/// Kubernetes rollout of the model list always did: no restart, no dropped
/// in-flight generations.
pub fn spawn_poller<S: SnapshotSource + 'static>(
    source: S,
    state: Arc<AppState>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let have = Some(state.snapshot.load().version).filter(|v| *v != 0);
            match source.fetch(have).await {
                Ok(Some(next)) => match state.apply_snapshot(next) {
                    Ok(n) => tracing::info!(backends = n, "snapshot changed, registry rebuilt"),
                    Err(e) => tracing::error!(
                        error = %e,
                        "snapshot changed but registry rebuild failed; keeping previous routing table"
                    ),
                },
                Ok(None) => {}
                Err(e) => {
                    // Expected whenever the control plane is down. The cached
                    // snapshot keeps serving, so this is a warning, not an error.
                    tracing::warn!(error = %e, "snapshot refresh failed; serving the cached policy");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::file::FileSource;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    struct Counting {
        calls: Arc<AtomicU64>,
        version: u64,
    }

    impl SnapshotSource for Counting {
        async fn fetch(&self, have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if have == Some(self.version) {
                return Ok(None);
            }
            let s = Snapshot {
                version: self.version,
                ..Snapshot::default()
            };
            Ok(Some(s))
        }
    }

    #[tokio::test]
    async fn the_poller_stops_swapping_once_the_version_is_current() {
        let calls = Arc::new(AtomicU64::new(0));
        let src = Counting {
            calls: Arc::clone(&calls),
            version: 7,
        };
        let cell = Arc::new(arc_swap::ArcSwap::from_pointee(Snapshot::default()));

        poll_once(&src, &cell).await.unwrap();
        assert_eq!(cell.load().version, 7);
        // Second poll returns None, so the stored Arc must not be replaced.
        let before = Arc::as_ptr(&cell.load_full());
        poll_once(&src, &cell).await.unwrap();
        assert_eq!(Arc::as_ptr(&cell.load_full()), before);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    /// The other half of the story above: an actual edit on disk, not a fake
    /// source, must also be picked up. This is the exact mechanism a
    /// ConfigMap edit relies on in `File` mode.
    #[tokio::test]
    async fn poll_once_picks_up_a_changed_file_on_disk() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "model_list:\n  - model_name: a\n    litellm_params: {{ api_base: http://h:8000/v1 }}\n"
        )
        .unwrap();
        f.flush().unwrap();

        let src = FileSource::new(f.path().into());
        let cell = ArcSwap::from_pointee(Snapshot::default());
        poll_once(&src, &cell).await.unwrap();
        assert_eq!(cell.load().models[0].name, "a");

        // Overwrite in place, as a ConfigMap edit would.
        std::fs::write(
            f.path(),
            "model_list:\n  - model_name: b\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        )
        .unwrap();

        poll_once(&src, &cell).await.unwrap();
        assert_eq!(cell.load().models[0].name, "b");
    }

    #[tokio::test]
    async fn with_legacy_master_key_merges_into_every_fetch() {
        struct Empty;
        impl SnapshotSource for Empty {
            async fn fetch(&self, _have: Option<u64>) -> anyhow::Result<Option<Snapshot>> {
                Ok(Some(Snapshot::default()))
            }
        }
        let src = WithLegacyMasterKey::new(Empty, Some("sk-legacy".to_string()));
        let snap = src.fetch(None).await.unwrap().unwrap();
        assert!(snap
            .authenticate("sk-legacy", std::time::SystemTime::now())
            .is_ok());
    }
}
