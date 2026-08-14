//! Where the data plane gets its policy.
//!
//! Two implementations, one trait, and forwarding cannot tell them apart:
//! `File` for a proxy with no control plane at all, `Http` for a proxy against
//! one. (`--role all` is not a third: it builds its snapshot from the database
//! in-process, so it has no source to poll.) This module also owns the polling
//! loop (`spawn_poller`) that both of those use to keep the snapshot — and the
//! routing `Registry` built from it — current without a restart.

pub mod file;
pub mod http;

use crate::snapshot::Snapshot;
use crate::state::AppState;
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
                    Ok(n) => {
                        // A snapshot can be what first makes escalation
                        // reachable, and the model load must not land on the
                        // next user's request. No-op once loaded, or when no
                        // active refined class needs it.
                        #[cfg(feature = "classifier-tier2")]
                        state.warm_refined_tier();
                        tracing::info!(backends = n, "snapshot changed, registry rebuilt");
                    }
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

    /// A ConfigMap edit, end to end: an edit on disk reaches `state.snapshot`
    /// with no restart, through the same `apply_snapshot` production uses.
    ///
    /// This used to be tested against a bare `ArcSwap` through a `poll_once`
    /// helper that existed only for the tests — which meant the one thing
    /// worth guarding, that a reload goes through the single write path and
    /// rebuilds the registry with it, was the one thing the test bypassed.
    #[tokio::test]
    async fn an_edit_on_disk_reaches_the_live_snapshot() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            f.path(),
            "model_list:\n  - model_name: a\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        )
        .unwrap();

        let state = Arc::new(AppState::for_test());
        let first = FileSource::new(f.path().into())
            .fetch(None)
            .await
            .unwrap()
            .unwrap();
        state.apply_snapshot(first).unwrap();
        assert_eq!(state.snapshot.load().models[0].name, "a");

        spawn_poller(
            FileSource::new(f.path().into()),
            Arc::clone(&state),
            Duration::from_millis(5),
        );

        // Overwrite in place, as a ConfigMap edit would.
        std::fs::write(
            f.path(),
            "model_list:\n  - model_name: b\n    litellm_params: { api_base: http://h:8000/v1 }\n",
        )
        .unwrap();

        for _ in 0..200 {
            if state.snapshot.load().models[0].name == "b" {
                // The registry is rebuilt in the same call, never separately.
                assert_eq!(state.registry.load().backends().len(), 1);
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the poller never picked up the edited file");
    }
}
