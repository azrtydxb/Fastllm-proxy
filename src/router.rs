//! Backend selection.
//!
//! The default policy is **cache-affinity with a load escape hatch**, which is
//! the thing plain round-robin gets wrong on prefix-caching engines.
//!
//! vLLM and SGLang both keep a prefix/radix KV cache. Two requests sharing a
//! system prompt cost far less on the *same* node than split across two, because
//! the second one reuses the first's cached prefix instead of prefilling it
//! again. A round-robin balancer alternates them by construction, so every
//! request pays full prefill and aggregate throughput drops even though the
//! nodes look evenly loaded. Adding a second node can make throughput *worse*.
//!
//! So: hash a prefix of the request, remember which backend served that prefix
//! last, and send it back there — unless that backend is meaningfully more
//! loaded than the least-loaded one, in which case take the cache miss and
//! rebalance. `balance_abs` / `balance_rel` set how much imbalance is tolerated
//! before cache locality is given up.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::registry::{Backend, BackendUid, Pool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Policy {
    /// Prefix-affinity, falling back to least-loaded when a node runs hot.
    CacheAffinity,
    /// Always the fewest in-flight requests. Cache-blind.
    LeastLoaded,
    /// Strict rotation. Cache-blind; present mainly as a baseline to measure against.
    RoundRobin,
}

pub struct Router {
    policy: Policy,
    affinity: AffinityCache,
    prefix_bytes: usize,
    balance_abs: usize,
    balance_rel: f64,
    rr: AtomicUsize,
}

impl Router {
    pub fn new(
        policy: Policy,
        affinity_slots: usize,
        prefix_bytes: usize,
        balance_abs: usize,
        balance_rel: f64,
    ) -> Self {
        Self {
            policy,
            affinity: AffinityCache::new(affinity_slots),
            prefix_bytes,
            balance_abs,
            balance_rel,
            rr: AtomicUsize::new(0),
        }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Affinity key for a request.
    ///
    /// Hashes the leading bytes of the *raw* body rather than parsed fields.
    /// OpenAI clients serialise `model` and `messages` first, so the first
    /// couple of KiB covers the system prompt and opening turns — exactly the
    /// span the engine's radix cache keys on — at the cost of one pass over a
    /// small byte slice and no allocation.
    ///
    /// Field order is not guaranteed by JSON, but it is stable for a given
    /// client, which is all affinity needs. A client that reorders per request
    /// degrades to least-loaded rather than misrouting.
    pub fn prefix_key(&self, body: &[u8]) -> u64 {
        let n = body.len().min(self.prefix_bytes);
        fxhash(&body[..n])
    }

    /// Choose a backend, skipping any already tried for this request.
    ///
    /// Returns `None` when every healthy backend has been exhausted.
    pub fn pick(&self, pool: &Pool, prefix: u64, exclude: &[BackendUid]) -> Option<Arc<Backend>> {
        let candidates: Vec<&Arc<Backend>> = pool
            .iter()
            .filter(|b| b.is_healthy() && !exclude.contains(&b.uid))
            .collect();

        // Everything healthy has already failed — fall back to unhealthy
        // backends rather than returning an error. A stale health flag should
        // not turn a recoverable request into a 503.
        let candidates = if candidates.is_empty() {
            let fallback: Vec<&Arc<Backend>> =
                pool.iter().filter(|b| !exclude.contains(&b.uid)).collect();
            if fallback.is_empty() {
                return None;
            }
            fallback
        } else {
            candidates
        };

        if candidates.len() == 1 {
            return Some(Arc::clone(candidates[0]));
        }

        let (chosen, remember) = match self.policy {
            Policy::RoundRobin => {
                let n = self.rr.fetch_add(1, Ordering::Relaxed);
                (candidates[n % candidates.len()], false)
            }
            Policy::LeastLoaded => (least_loaded(&candidates, &self.rr), false),
            Policy::CacheAffinity => self.pick_affine(&candidates, prefix),
        };

        if remember {
            self.affinity.put(prefix, chosen.uid);
        }
        Some(Arc::clone(chosen))
    }

    /// Returns the chosen backend and whether it should claim this prefix.
    ///
    /// A load-driven deviation deliberately does *not* claim it: the prefix's
    /// KV cache still lives on the original node, so overwriting the mapping
    /// would let a momentary spike migrate a hot prefix permanently and lose
    /// the cache twice — once on the way out, once on the way back.
    fn pick_affine<'a>(
        &self,
        candidates: &[&'a Arc<Backend>],
        prefix: u64,
    ) -> (&'a Arc<Backend>, bool) {
        let min_load = candidates.iter().map(|b| b.inflight()).min().unwrap_or(0);
        let slack = self
            .balance_abs
            .max((min_load as f64 * (self.balance_rel - 1.0)).ceil() as usize);

        if let Some(uid) = self.affinity.get(prefix) {
            if let Some(sticky) = candidates.iter().find(|b| b.uid == uid) {
                if sticky.inflight() <= min_load.saturating_add(slack) {
                    return (sticky, false);
                }
                // Too hot right now. Shed this one request, keep the mapping.
                return (least_loaded_for(candidates, prefix), false);
            }
        }
        // Genuine cold miss: claim the prefix.
        (least_loaded_for(candidates, prefix), true)
    }
}

/// Least loaded, with ties broken by the prefix hash.
///
/// Hash tie-breaking rather than round-robin matters while the cluster is idle:
/// every backend reads as zero in-flight, so a positional tie-break would send
/// every cold prefix to the same node. Spreading by hash keeps the choice
/// deterministic — the same prefix picks the same node even before it is
/// cached — while distributing distinct prefixes across the cluster.
fn least_loaded_for<'a>(candidates: &[&'a Arc<Backend>], prefix: u64) -> &'a Arc<Backend> {
    let min = candidates.iter().map(|b| b.inflight()).min().unwrap_or(0);
    let tied: Vec<&'a Arc<Backend>> = candidates
        .iter()
        .copied()
        .filter(|b| b.inflight() == min)
        .collect();
    let tied = if tied.is_empty() {
        candidates
    } else {
        &tied[..]
    };
    tied[(prefix % tied.len() as u64) as usize]
}

/// Least loaded, with ties broken by rotation. Used by the cache-blind policy,
/// where there is no prefix to spread on.
fn least_loaded<'a>(candidates: &[&'a Arc<Backend>], rr: &AtomicUsize) -> &'a Arc<Backend> {
    let min = candidates.iter().map(|b| b.inflight()).min().unwrap_or(0);
    let tied: Vec<&'a Arc<Backend>> = candidates
        .iter()
        .copied()
        .filter(|b| b.inflight() == min)
        .collect();
    let tied = if tied.is_empty() {
        candidates
    } else {
        &tied[..]
    };
    tied[rr.fetch_add(1, Ordering::Relaxed) % tied.len()]
}

/// Direct-mapped, lock-free prefix -> backend cache.
///
/// A full LRU is not worth it here: the access pattern is "a handful of hot
/// system prompts", collisions merely cost a cache miss, and every operation
/// has to sit on the per-request hot path. One relaxed atomic load and one
/// relaxed store beats any map with a lock in front of it.
///
/// Slot encoding: `(tag << 16) | uid`, where `tag` is the upper 48 bits of the
/// hash. Zero means empty, so the one hash that would encode to zero is simply
/// never cached.
struct AffinityCache {
    slots: Box<[AtomicU64]>,
    mask: usize,
}

impl AffinityCache {
    fn new(requested: usize) -> Self {
        let n = requested.max(64).next_power_of_two();
        let slots = (0..n).map(|_| AtomicU64::new(0)).collect::<Vec<_>>();
        Self {
            slots: slots.into_boxed_slice(),
            mask: n - 1,
        }
    }

    #[inline]
    fn encode(hash: u64, uid: BackendUid) -> u64 {
        ((hash >> 16) << 16) | uid as u64
    }

    #[inline]
    fn get(&self, hash: u64) -> Option<BackendUid> {
        let slot = self.slots[hash as usize & self.mask].load(Ordering::Relaxed);
        if slot == 0 {
            return None;
        }
        // Confirm the slot belongs to this hash and not a colliding one.
        if slot >> 16 == hash >> 16 {
            Some(slot as u16)
        } else {
            None
        }
    }

    #[inline]
    fn put(&self, hash: u64, uid: BackendUid) {
        let encoded = Self::encode(hash, uid);
        if encoded == 0 {
            return;
        }
        self.slots[hash as usize & self.mask].store(encoded, Ordering::Relaxed);
    }
}

/// FxHash over a byte slice — the rustc hasher. Not cryptographic; chosen
/// because this runs on every request and only needs good dispersion.
fn fxhash(bytes: &[u8]) -> u64 {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
    let mut hash: u64 = 0;
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        hash = (hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
    let mut tail: u64 = 0;
    for (i, b) in chunks.remainder().iter().enumerate() {
        tail |= (*b as u64) << (i * 8);
    }
    hash = (hash.rotate_left(5) ^ tail).wrapping_mul(SEED);
    // Final avalanche so the high bits used for the tag are well mixed.
    hash ^= hash >> 32;
    hash = hash.wrapping_mul(SEED);
    hash ^ (hash >> 29)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FileConfig;
    use crate::registry::{Interner, Registry};

    fn two_node_pool() -> Pool {
        let cfg: FileConfig = serde_yaml::from_str(
            r#"
model_list:
  - model_name: m
    litellm_params: { api_base: "http://10.0.0.1:8000/v1" }
  - model_name: m
    litellm_params: { api_base: "http://10.0.0.2:8000/v1" }
"#,
        )
        .unwrap();
        let reg = Registry::build(&cfg, &Interner::default(), None).unwrap();
        Arc::clone(reg.pool("m").unwrap())
    }

    fn router(policy: Policy) -> Router {
        Router::new(policy, 1024, 2048, 8, 1.5)
    }

    #[test]
    fn affinity_keeps_a_prefix_on_one_node() {
        let pool = two_node_pool();
        let r = router(Policy::CacheAffinity);
        let key = r.prefix_key(b"{\"model\":\"m\",\"messages\":[{\"role\":\"system\"}]}");
        let first = r.pick(&pool, key, &[]).unwrap();
        for _ in 0..50 {
            let again = r.pick(&pool, key, &[]).unwrap();
            assert_eq!(again.uid, first.uid, "same prefix must stick to one node");
        }
    }

    #[test]
    fn distinct_prefixes_spread_across_nodes() {
        let pool = two_node_pool();
        let r = router(Policy::CacheAffinity);
        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            let body = format!("{{\"model\":\"m\",\"messages\":[{{\"content\":\"{i}\"}}]}}");
            seen.insert(
                r.pick(&pool, r.prefix_key(body.as_bytes()), &[])
                    .unwrap()
                    .uid,
            );
        }
        assert_eq!(seen.len(), 2, "cold prefixes should use both nodes");
    }

    #[test]
    fn affinity_yields_when_the_sticky_node_is_overloaded() {
        use crate::registry::InflightGuard;
        let pool = two_node_pool();
        let r = router(Policy::CacheAffinity);
        let key = r.prefix_key(b"hot-prefix");
        let sticky = r.pick(&pool, key, &[]).unwrap();

        // Pile load on the sticky node past balance_abs.
        let guards: Vec<_> = (0..20)
            .map(|_| InflightGuard::acquire(Arc::clone(&sticky)))
            .collect();
        let picked = r.pick(&pool, key, &[]).unwrap();
        assert_ne!(
            picked.uid, sticky.uid,
            "should shed load rather than hold affinity"
        );

        drop(guards);
        // Once the imbalance clears, the warm node wins again.
        assert_eq!(r.pick(&pool, key, &[]).unwrap().uid, sticky.uid);
    }

    #[test]
    fn small_imbalance_does_not_break_affinity() {
        use crate::registry::InflightGuard;
        let pool = two_node_pool();
        let r = router(Policy::CacheAffinity);
        let key = r.prefix_key(b"warm");
        let sticky = r.pick(&pool, key, &[]).unwrap();
        // Under balance_abs=8, affinity is worth more than perfect balance.
        let _guards: Vec<_> = (0..5)
            .map(|_| InflightGuard::acquire(Arc::clone(&sticky)))
            .collect();
        assert_eq!(r.pick(&pool, key, &[]).unwrap().uid, sticky.uid);
    }

    #[test]
    fn exclude_forces_a_different_backend() {
        let pool = two_node_pool();
        let r = router(Policy::CacheAffinity);
        let key = r.prefix_key(b"x");
        let first = r.pick(&pool, key, &[]).unwrap();
        let second = r.pick(&pool, key, &[first.uid]).unwrap();
        assert_ne!(first.uid, second.uid);
        assert!(r.pick(&pool, key, &[first.uid, second.uid]).is_none());
    }

    #[test]
    fn unhealthy_backends_are_skipped_but_used_as_last_resort() {
        let pool = two_node_pool();
        let r = router(Policy::LeastLoaded);
        pool[0].mark_probe_failed(1);
        for _ in 0..10 {
            assert_eq!(r.pick(&pool, 0, &[]).unwrap().uid, pool[1].uid);
        }
        // With the only healthy node excluded, the unhealthy one still beats a 503.
        assert_eq!(r.pick(&pool, 0, &[pool[1].uid]).unwrap().uid, pool[0].uid);
    }

    #[test]
    fn round_robin_alternates() {
        let pool = two_node_pool();
        let r = router(Policy::RoundRobin);
        let a = r.pick(&pool, 0, &[]).unwrap().uid;
        let b = r.pick(&pool, 0, &[]).unwrap().uid;
        assert_ne!(a, b);
        assert_eq!(r.pick(&pool, 0, &[]).unwrap().uid, a);
    }

    #[test]
    fn affinity_cache_detects_collisions() {
        let cache = AffinityCache::new(64);
        cache.put(0x1234_5678_9abc_def0, 7);
        assert_eq!(cache.get(0x1234_5678_9abc_def0), Some(7));
        // Same slot index, different tag => miss, not a wrong answer.
        let colliding = 0x1234_5678_9abc_def0u64 ^ (1 << 20);
        assert_eq!(
            colliding as usize & cache.mask,
            0x1234_5678_9abc_def0usize & cache.mask
        );
        assert_eq!(cache.get(colliding), None);
    }

    #[test]
    fn hash_is_prefix_sensitive_and_suffix_insensitive() {
        let r = Router::new(Policy::CacheAffinity, 64, 16, 8, 1.5);
        // Same first 16 bytes, different tail => same key (that is the point).
        assert_eq!(
            r.prefix_key(b"0123456789abcdefAAAA"),
            r.prefix_key(b"0123456789abcdefBBBB")
        );
        assert_ne!(
            r.prefix_key(b"0123456789abcdef"),
            r.prefix_key(b"0123456789abcdeg")
        );
    }
}
