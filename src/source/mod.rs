//! Where the data plane gets its policy.
//!
//! Three implementations, one trait, and forwarding cannot tell them apart:
//! `File` for a proxy with no control plane at all, `Local` for the
//! single-process role, `Http` for a proxy against a control plane.

pub mod file;

use crate::snapshot::Snapshot;

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
