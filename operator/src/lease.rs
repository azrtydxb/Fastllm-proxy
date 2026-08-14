//! Leader election over a `coordination.k8s.io/v1` Lease.
//!
//! # Why this exists rather than `replicas: 1`
//!
//! Pinning the Deployment to one replica is not availability, it is the
//! absence of it: a node drain, an OOM kill or a failed rollout leaves the
//! cluster with no controller at all, and nothing reconciles until the pod
//! comes back. The reason it was pinned is real, though — two controllers
//! applying the same objects fight over the same fields for ever, and the
//! rollout never settles.
//!
//! A Lease resolves both. Every replica runs; exactly one holds the Lease and
//! reconciles, and the others sit in `acquire` waiting for it to expire. A
//! lost leader stops reconciling and exits, so the container restarts clean
//! rather than continuing to write with a stale claim.
//!
//! # Why it is written here rather than taken from a crate
//!
//! Roughly eighty lines against a dependency that would need its own version
//! bump every time `kube` moves. The algorithm is: hold if the record is
//! yours, take it if it has expired, otherwise wait — with the API server's
//! optimistic concurrency (`resourceVersion` on a full replace) doing the
//! only hard part, which is making two simultaneous takeovers impossible.

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::chrono::{DateTime, Utc};
use kube::api::{Api, PostParams};
use std::time::Duration;
use tracing::{info, warn};

pub struct LeaderElector {
    api: Api<Lease>,
    name: String,
    /// Unique per process. The pod name, which the downward API supplies and
    /// Kubernetes guarantees unique within a namespace at any one time.
    identity: String,
    /// How long a claim survives without a renewal. Longer than `renew` by
    /// enough that one slow API call does not hand the lease away.
    duration: Duration,
    renew: Duration,
}

#[derive(Debug)]
pub enum Lost {
    /// Somebody else's identity is in the record. Never expected while we
    /// renew on time, and a hard stop when it happens: two writers is the
    /// state this exists to prevent.
    Stolen(String),
    Api(kube::Error),
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stolen(who) => write!(f, "lease taken over by {who:?}"),
            Self::Api(e) => write!(f, "lease renewal failed: {e}"),
        }
    }
}

impl LeaderElector {
    pub fn new(api: Api<Lease>, name: impl Into<String>, identity: impl Into<String>) -> Self {
        Self {
            api,
            name: name.into(),
            identity: identity.into(),
            duration: Duration::from_secs(15),
            renew: Duration::from_secs(5),
        }
    }

    fn spec(&self, transitions: i32, acquired: Option<MicroTime>) -> LeaseSpec {
        let now = MicroTime(Utc::now());
        LeaseSpec {
            holder_identity: Some(self.identity.clone()),
            lease_duration_seconds: Some(self.duration.as_secs() as i32),
            acquire_time: Some(acquired.unwrap_or_else(|| now.clone())),
            renew_time: Some(now),
            lease_transitions: Some(transitions),
            ..Default::default()
        }
    }

    /// Block until this process holds the lease.
    pub async fn acquire(&self) -> Result<(), kube::Error> {
        loop {
            match self.try_acquire().await {
                Ok(true) => {
                    info!(identity = %self.identity, "acquired leadership");
                    return Ok(());
                }
                Ok(false) => {}
                // A transient API error is not a reason to give up on
                // becoming leader — the current one may be gone.
                Err(e) => warn!(error = %e, "leader election attempt failed, retrying"),
            }
            tokio::time::sleep(self.renew).await;
        }
    }

    async fn try_acquire(&self) -> Result<bool, kube::Error> {
        let Some(existing) = self.api.get_opt(&self.name).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(self.name.clone()),
                    ..Default::default()
                },
                spec: Some(self.spec(0, None)),
            };
            // A 409 here means somebody created it in the same instant; the
            // next pass reads theirs and waits, which is correct.
            return match self.api.create(&PostParams::default(), &lease).await {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(r)) if r.code == 409 => Ok(false),
                Err(e) => Err(e),
            };
        };

        let spec = existing.spec.clone().unwrap_or_default();
        let holder = spec.holder_identity.clone().unwrap_or_default();
        let ours = holder == self.identity;
        if !ours && !expired(&spec) {
            return Ok(false);
        }

        let transitions = spec.lease_transitions.unwrap_or(0) + i32::from(!ours);
        let acquired = ours.then_some(spec.acquire_time.clone()).flatten();
        let mut next = existing;
        next.spec = Some(self.spec(transitions, acquired));
        // `replace` carries the resourceVersion we read, so two candidates
        // taking over an expired lease at the same moment cannot both win:
        // the loser gets a 409 and waits.
        match self
            .api
            .replace(&self.name, &PostParams::default(), &next)
            .await
        {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(r)) if r.code == 409 => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Renew for as long as we hold it. Returns only when leadership is lost,
    /// which the caller is expected to treat as fatal.
    pub async fn keep(&self) -> Lost {
        loop {
            tokio::time::sleep(self.renew).await;
            let current = match self.api.get_opt(&self.name).await {
                Ok(Some(l)) => l,
                // Deleted underneath us: recreate on the next pass by
                // treating it as a takeover attempt.
                Ok(None) => match self.try_acquire().await {
                    Ok(true) => continue,
                    Ok(false) => return Lost::Stolen("another replica".into()),
                    Err(e) => return Lost::Api(e),
                },
                Err(e) => return Lost::Api(e),
            };
            let spec = current.spec.clone().unwrap_or_default();
            let holder = spec.holder_identity.clone().unwrap_or_default();
            if holder != self.identity {
                return Lost::Stolen(holder);
            }
            let transitions = spec.lease_transitions.unwrap_or(0);
            let acquired = spec.acquire_time.clone();
            let mut next = current;
            next.spec = Some(self.spec(transitions, acquired));
            match self
                .api
                .replace(&self.name, &PostParams::default(), &next)
                .await
            {
                Ok(_) => {}
                // Somebody else wrote it between our read and our write. We
                // may still be the holder, so try again rather than resign.
                Err(kube::Error::Api(r)) if r.code == 409 => continue,
                Err(e) => return Lost::Api(e),
            }
        }
    }
}

/// Has the record gone stale? `renewTime + leaseDurationSeconds` in the past
/// means the holder stopped renewing and the lease is free.
fn expired(spec: &LeaseSpec) -> bool {
    let Some(renewed) = spec.renew_time.as_ref().map(|t| t.0) else {
        return true;
    };
    let duration = spec.lease_duration_seconds.unwrap_or(15).max(1) as i64;
    let deadline: DateTime<Utc> = renewed + k8s_openapi::chrono::Duration::seconds(duration);
    Utc::now() > deadline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_renewed_secs_ago(secs: i64, duration: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: Some("other".into()),
            lease_duration_seconds: Some(duration),
            renew_time: Some(MicroTime(
                Utc::now() - k8s_openapi::chrono::Duration::seconds(secs),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn a_lease_renewed_within_its_duration_is_held() {
        assert!(!expired(&spec_renewed_secs_ago(3, 15)));
    }

    #[test]
    fn a_lease_past_its_duration_is_free() {
        assert!(expired(&spec_renewed_secs_ago(20, 15)));
    }

    /// A record with no `renewTime` at all is not a claim on anything. Left
    /// un-takeable it would deadlock every replica for ever.
    #[test]
    fn a_record_with_no_renewal_time_is_free() {
        assert!(expired(&LeaseSpec::default()));
    }

    /// A zero or negative duration would make every lease permanently valid
    /// or permanently expired depending on rounding; clamp and move on.
    #[test]
    fn a_nonsense_duration_does_not_make_a_lease_immortal() {
        assert!(expired(&spec_renewed_secs_ago(5, 0)));
    }
}
