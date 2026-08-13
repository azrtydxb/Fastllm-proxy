//! Outbound notifications for the things an operator wants to be told about
//! rather than to discover.
//!
//! The gateway already publishes everything it knows — `/metrics` for
//! scraping, `/admin/fleet` for polling, `usage_events` for querying. All of
//! those require somebody to be looking. This is the other direction: a
//! backend going down at 3am is worth a message, and nobody is looking at
//! 3am.
//!
//! # Shaped like `crate::usage`, on purpose
//!
//! Bounded queue, background flush, drop-on-full with a counter. That module
//! made the argument already and it applies unchanged here: emitting a
//! notification must never block, retry-storm, or grow memory without bound,
//! however slow or unreachable the receiving endpoint is. A webhook receiver
//! that has fallen over must not be able to take the control plane with it.
//!
//! One difference: usage events are billing data and their loss is recorded
//! as a defect. A dropped notification is a missed message about a condition
//! that is still true and still visible everywhere else, so the queue here is
//! deliberately small — holding a thousand stale alerts to deliver in a burst
//! once the receiver returns is worse than dropping them.

use crate::upstream::Upstream;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Something worth telling somebody about.
///
/// Deliberately few. Every event here is one an operator would act on: a
/// backend is gone, the published configuration has stopped tracking the
/// database, a caller has hit a wall. Adding "a key was created" would turn
/// this into a duplicate of the audit log delivered over HTTP, which is a
/// worse audit log and a noisier channel — and a channel people mute is
/// indistinguishable from one that does not exist.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A replica newly reports a backend as unhealthy.
    ///
    /// Per replica, not fleet-wide, because the two failures need different
    /// responses: every replica losing a backend is a dead backend, and one
    /// replica losing it is a partition. Merging them here would delete the
    /// distinction before anyone saw it.
    BackendDown {
        replica: String,
        api_base: String,
        model: String,
    },
    /// The same backend reporting healthy again. Sent so a receiver can close
    /// its own incident rather than leaving one open until a human looks.
    BackendRecovered {
        replica: String,
        api_base: String,
        model: String,
    },
    /// A snapshot rebuild failed after a write committed, so the database and
    /// the published configuration have diverged. Already counted on
    /// `GET /admin/health`; this is the push half.
    SnapshotRebuildFailed { error: String, consecutive: u64 },
}

#[derive(Serialize)]
struct Envelope<'a> {
    /// RFC 3339, from the sender rather than the receiver, so an alert
    /// delayed in a queue still says when the thing happened.
    at: String,
    #[serde(flatten)]
    event: &'a Event,
}

pub struct WebhookSender {
    tx: Option<mpsc::Sender<Event>>,
    dropped: Arc<AtomicU64>,
}

impl WebhookSender {
    /// A sender that discards everything, for a deployment with no webhook
    /// configured — which is the default. Same shape as
    /// `UsageReporter::disabled`, so no call site needs to know whether
    /// notifications are on.
    pub fn disabled() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Notifications discarded because the queue was full — a receiver that
    /// cannot keep up, or one that is down. Exposed so that "we sent nothing"
    /// and "we could not send" are distinguishable.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn send(&self, event: Event) {
        let Some(tx) = &self.tx else { return };
        if tx.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Start the background sender.
///
/// `url` is the only required setting. `secret`, when given, signs each body
/// with HMAC-SHA256 in an `x-fastllm-signature` header so a receiver can tell
/// a real notification from anything else that finds the URL — a webhook
/// endpoint is by nature reachable by whoever learns its address.
pub fn spawn(
    url: String,
    secret: Option<String>,
    upstream: Arc<Upstream>,
    queue_capacity: usize,
) -> WebhookSender {
    let (tx, mut rx) = mpsc::channel::<Event>(queue_capacity);
    let dropped = Arc::new(AtomicU64::new(0));

    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let body = match serde_json::to_vec(&Envelope {
                at: chrono::Utc::now().to_rfc3339(),
                event: &event,
            }) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "could not encode webhook payload");
                    continue;
                }
            };

            let mut builder = Request::builder()
                .method("POST")
                .uri(&url)
                .header("content-type", "application/json");
            if let Some(secret) = &secret {
                builder = builder.header("x-fastllm-signature", sign(secret, &body));
            }
            let Ok(req) = builder.body(Full::new(Bytes::from(body))) else {
                tracing::warn!(%url, "webhook URL is not a valid request target");
                continue;
            };

            // One attempt, with a timeout, and no retry. A receiver that is
            // down will still be down in five seconds, and a retry loop here
            // would turn one unreachable endpoint into a queue that never
            // drains — the condition being reported is still true and still
            // visible on /metrics and /admin/fleet.
            match tokio::time::timeout(Duration::from_secs(5), upstream.request(req)).await {
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    // Drain so the connection can be reused rather than shut.
                    let _ = resp.into_body().collect().await;
                    if !status.is_success() {
                        tracing::warn!(%status, "webhook receiver rejected a notification");
                    }
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "webhook delivery failed"),
                Err(_) => tracing::warn!("webhook delivery timed out"),
            }
        }
    });

    WebhookSender {
        tx: Some(tx),
        dropped,
    }
}

/// HMAC-SHA256, hex, `sha256=` prefixed — the shape GitHub popularised and
/// most receivers already know how to check.
fn sign(secret: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;

    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        let digest = Sha256::digest(secret.as_bytes());
        key[..digest.len()].copy_from_slice(&digest);
    } else {
        key[..secret.len()].copy_from_slice(secret.as_bytes());
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(body);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    let mac = outer.finalize();

    let mut out = String::with_capacity(7 + mac.len() * 2);
    out.push_str("sha256=");
    for b in mac {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned against RFC 4231's test case 2, so this is checked against the
    /// standard rather than against itself. A hand-rolled HMAC that is
    /// self-consistent but wrong would still verify against a second copy of
    /// the same mistake, and every receiver would reject every notification.
    #[test]
    fn hmac_matches_the_rfc_4231_vector() {
        assert_eq!(
            sign("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// A secret longer than the block size is hashed first, per the spec.
    /// Getting this wrong only shows up for long secrets, which is exactly
    /// the kind someone generates with `openssl rand -hex 64`.
    #[test]
    fn a_long_secret_is_hashed_to_the_block_size() {
        let long = "a".repeat(200);
        let sig = sign(&long, b"payload");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), 7 + 64);
        // And it must differ from the truncated-secret answer a naive
        // implementation would produce.
        assert_ne!(sig, sign(&"a".repeat(64), b"payload"));
    }

    #[test]
    fn a_disabled_sender_drops_nothing_and_never_panics() {
        let s = WebhookSender::disabled();
        s.send(Event::BackendDown {
            replica: "r".into(),
            api_base: "http://x".into(),
            model: "m".into(),
        });
        assert_eq!(s.dropped(), 0, "a disabled sender is not a failing sender");
    }
}
