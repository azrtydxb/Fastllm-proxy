//! An exact-match response cache.
//!
//! # Why this is the one feature that makes a gateway faster
//!
//! Everything else here adds latency, however little, and the design work is
//! keeping that number small. A cache hit is the only path where routing
//! through this proxy is *faster* than calling the provider directly — no
//! network hop, no generation, microseconds instead of seconds.
//!
//! # Off unless asked for, per model
//!
//! Caching changes semantics. Two identical requests to a model at
//! `temperature > 0` are supposed to be able to differ, and a gateway that
//! silently returns the first answer to both has changed what the caller
//! asked for. So it is opt-in per model (`models.cache_ttl_seconds`), and a
//! deployment that sets nothing pays nothing — not even the hash, which is
//! only computed once a model is known to have caching on.
//!
//! # What is cached, and what is deliberately not
//!
//! **Non-streaming 2xx responses only.**
//!
//! Not streaming, because caching a stream means buffering the whole response
//! before any of it reaches the client — turning the one path this proxy is
//! built to keep incremental into a batch operation, and holding a generation's
//! worth of bytes per in-flight request to do it. The natural fit for exact-
//! match caching is embeddings and short non-streaming completions anyway,
//! which are precisely the requests that repeat.
//!
//! Not errors. A 429 or a 502 is a statement about *now*, and serving it from
//! a cache would keep a provider's bad minute alive long after it ended.
//!
//! # Why a per-process cache rather than a shared one
//!
//! A shared cache means a network call, and the request path performs no I/O
//! (`tests/no_io_on_hot_path.rs`). A per-process cache gives a lower hit rate
//! across replicas and is honest about it; the alternative is a Redis hop that
//! costs a millisecond to save a second, which is still a win but not one this
//! design can make without giving up its central invariant.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;

/// A cached response.
#[derive(Clone)]
pub struct Entry {
    pub content_type: Option<String>,
    pub body: Bytes,
    expires_at: Instant,
}

/// Bounded by both entries and bytes.
///
/// Two limits rather than one because they fail differently: a thousand
/// embedding responses is nothing, and a thousand long completions is hundreds
/// of megabytes. Either alone leaves the other unbounded.
pub struct ResponseCache {
    entries: Mutex<HashMap<u64, Entry>>,
    max_entries: usize,
    max_bytes: usize,
    /// Cheap to read without the lock, for `/metrics`.
    bytes: AtomicU64,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub stores: AtomicU64,
}

/// Above this, a response is not worth caching: it is a rare shape, and one of
/// them can evict a great many useful small ones.
const MAX_ENTRY_BYTES: usize = 512 * 1024;

impl ResponseCache {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_entries,
            max_bytes,
            bytes: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stores: AtomicU64::new(0),
        }
    }

    /// The key for a request: the model that will serve it, and the body.
    ///
    /// The **resolved** model, not the requested one — `auto` may route to
    /// different models for different callers or at different times, and
    /// keying on the name the client typed would serve one model's answer for
    /// another's.
    ///
    /// The whole body, not the routing prefix. The prefix hash exists to make
    /// two turns of one conversation land on the same backend, which is the
    /// opposite of what a cache key needs.
    pub fn key(model: &str, body: &[u8]) -> u64 {
        let mut h = fxhash(model.as_bytes());
        h = h.rotate_left(5) ^ fxhash(body);
        // Length is not implied by the hash and is cheap insurance against a
        // collision between two bodies of different sizes.
        h.rotate_left(7) ^ (body.len() as u64)
    }

    pub fn get(&self, key: u64, now: Instant) -> Option<Entry> {
        let mut entries = self.entries.lock();
        match entries.get(&key) {
            Some(e) if e.expires_at > now => {
                let hit = e.clone();
                drop(entries);
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(hit)
            }
            // Expired: removed on the way past rather than left for a sweep.
            // The only thing that reliably visits a dead entry is a request
            // that wanted it.
            Some(_) => {
                if let Some(old) = entries.remove(&key) {
                    self.bytes
                        .fetch_sub(old.body.len() as u64, Ordering::Relaxed);
                }
                drop(entries);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                drop(entries);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    pub fn put(
        &self,
        key: u64,
        content_type: Option<String>,
        body: Bytes,
        ttl: Duration,
        now: Instant,
    ) {
        if body.len() > MAX_ENTRY_BYTES || ttl.is_zero() {
            return;
        }
        let mut entries = self.entries.lock();

        // Full: drop everything expired, and if that is not enough, refuse to
        // store rather than evicting something live.
        //
        // No LRU. Tracking recency costs a write on every *hit*, which is the
        // path this whole module exists to keep fast, and the workload that
        // suits an exact-match cache — the same requests repeating — is served
        // as well by "keep what is already here" as by anything cleverer.
        if entries.len() >= self.max_entries
            || self.bytes.load(Ordering::Relaxed) as usize + body.len() > self.max_bytes
        {
            let before = entries.len();
            entries.retain(|_, e| e.expires_at > now);
            if before != entries.len() {
                let live: u64 = entries.values().map(|e| e.body.len() as u64).sum();
                self.bytes.store(live, Ordering::Relaxed);
            }
            if entries.len() >= self.max_entries
                || self.bytes.load(Ordering::Relaxed) as usize + body.len() > self.max_bytes
            {
                return;
            }
        }

        let len = body.len() as u64;
        let replaced = entries.insert(
            key,
            Entry {
                content_type,
                body,
                expires_at: now + ttl,
            },
        );
        if let Some(old) = replaced {
            self.bytes
                .fetch_sub(old.body.len() as u64, Ordering::Relaxed);
        }
        self.bytes.fetch_add(len, Ordering::Relaxed);
        self.stores.fetch_add(1, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Drop everything. Called when a snapshot changes a model's backends or
    /// its price: an answer cached from one provider must not be served as
    /// another's after a reconfiguration.
    pub fn clear(&self) {
        self.entries.lock().clear();
        self.bytes.store(0, Ordering::Relaxed);
    }
}

/// The same hash the router uses, over a whole body rather than a prefix.
fn fxhash(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
    let mut hash: u64 = 0;
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let v = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        hash = (hash.rotate_left(5) ^ v).wrapping_mul(SEED);
    }
    for &b in chunks.remainder() {
        hash = (hash.rotate_left(5) ^ u64::from(b)).wrapping_mul(SEED);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> ResponseCache {
        ResponseCache::new(4, 1024)
    }

    #[test]
    fn a_stored_response_comes_back_until_it_expires() {
        let c = cache();
        let now = Instant::now();
        let key = ResponseCache::key("m", b"body");
        c.put(
            key,
            Some("application/json".into()),
            Bytes::from_static(b"{}"),
            Duration::from_secs(60),
            now,
        );

        let hit = c.get(key, now).expect("stored");
        assert_eq!(hit.body, Bytes::from_static(b"{}"));
        assert_eq!(hit.content_type.as_deref(), Some("application/json"));

        assert!(
            c.get(key, now + Duration::from_secs(61)).is_none(),
            "past its ttl"
        );
        assert_eq!(c.len(), 0, "and it is gone, not merely hidden");
    }

    /// The resolved model is part of the key. `auto` can route two callers to
    /// different models, and keying on the name the client typed would serve
    /// one model's answer as another's.
    #[test]
    fn the_same_body_to_a_different_model_is_a_different_key() {
        assert_ne!(
            ResponseCache::key("qwen", b"body"),
            ResponseCache::key("claude", b"body")
        );
    }

    #[test]
    fn different_bodies_do_not_share_a_key() {
        assert_ne!(
            ResponseCache::key("m", br#"{"messages":[{"content":"a"}]}"#),
            ResponseCache::key("m", br#"{"messages":[{"content":"b"}]}"#)
        );
        // Length participates, so a prefix is not its own key.
        assert_ne!(
            ResponseCache::key("m", b"ab"),
            ResponseCache::key("m", b"a")
        );
    }

    #[test]
    fn a_full_cache_expires_the_dead_before_refusing() {
        let c = ResponseCache::new(2, 4096);
        let now = Instant::now();
        c.put(
            1,
            None,
            Bytes::from_static(b"x"),
            Duration::from_secs(1),
            now,
        );
        c.put(
            2,
            None,
            Bytes::from_static(b"y"),
            Duration::from_secs(1),
            now,
        );
        assert_eq!(c.len(), 2);

        // Full, and everything in it is still live: refuse rather than evict
        // something that may be about to be asked for.
        c.put(
            3,
            None,
            Bytes::from_static(b"z"),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(c.len(), 2, "a live entry is not evicted for a new one");

        // Once they expire, the room is reclaimed.
        let later = now + Duration::from_secs(2);
        c.put(
            3,
            None,
            Bytes::from_static(b"z"),
            Duration::from_secs(60),
            later,
        );
        assert_eq!(c.len(), 1);
        assert!(c.get(3, later).is_some());
    }

    /// A byte budget as well as an entry count, because they fail differently:
    /// a thousand embedding responses is nothing and a thousand completions is
    /// hundreds of megabytes.
    #[test]
    fn the_byte_budget_is_enforced_independently_of_the_entry_count() {
        let c = ResponseCache::new(100, 16);
        let now = Instant::now();
        c.put(
            1,
            None,
            Bytes::from_static(b"0123456789"),
            Duration::from_secs(60),
            now,
        );
        c.put(
            2,
            None,
            Bytes::from_static(b"0123456789"),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(c.len(), 1, "well under the entry cap, but over the bytes");
        assert_eq!(c.bytes(), 10);
    }

    #[test]
    fn an_oversized_response_is_not_cached_at_all() {
        let c = ResponseCache::new(100, 100 * 1024 * 1024);
        let now = Instant::now();
        let huge = Bytes::from(vec![0u8; MAX_ENTRY_BYTES + 1]);
        c.put(1, None, huge, Duration::from_secs(60), now);
        assert!(
            c.is_empty(),
            "one rare shape must not evict many useful ones"
        );
    }

    #[test]
    fn a_zero_ttl_stores_nothing() {
        let c = cache();
        c.put(
            1,
            None,
            Bytes::from_static(b"x"),
            Duration::ZERO,
            Instant::now(),
        );
        assert!(c.is_empty(), "ttl 0 is how a model says caching is off");
    }

    #[test]
    fn replacing_an_entry_does_not_double_count_its_bytes() {
        let c = cache();
        let now = Instant::now();
        c.put(
            1,
            None,
            Bytes::from_static(b"aaaa"),
            Duration::from_secs(60),
            now,
        );
        c.put(
            1,
            None,
            Bytes::from_static(b"bb"),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes(), 2, "the old entry's bytes were released");
    }

    #[test]
    fn hits_and_misses_are_counted_for_the_scrape() {
        let c = cache();
        let now = Instant::now();
        assert!(c.get(1, now).is_none());
        c.put(
            1,
            None,
            Bytes::from_static(b"x"),
            Duration::from_secs(60),
            now,
        );
        assert!(c.get(1, now).is_some());
        assert_eq!(c.hits.load(Ordering::Relaxed), 1);
        assert_eq!(c.misses.load(Ordering::Relaxed), 1);
        assert_eq!(c.stores.load(Ordering::Relaxed), 1);
    }

    /// A reconfiguration can point a model at a different provider. An answer
    /// cached from the old one must not be served as the new one's.
    #[test]
    fn clearing_releases_the_byte_accounting_too() {
        let c = cache();
        c.put(
            1,
            None,
            Bytes::from_static(b"xxxx"),
            Duration::from_secs(60),
            Instant::now(),
        );
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.bytes(), 0, "or the budget leaks across every reload");
    }
}
