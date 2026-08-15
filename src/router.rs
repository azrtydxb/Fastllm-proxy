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

use smallvec::SmallVec;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::registry::{Backend, BackendUid, Pool};

/// Candidate list for one routing decision. Pools are a handful of nodes, so
/// this stays on the stack and the hot path allocates nothing.
type Candidates<'a> = SmallVec<[&'a Arc<Backend>; 8]>;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Prefix-affinity, falling back to least-loaded when a node runs hot.
    CacheAffinity,
    /// Always the fewest in-flight requests. Cache-blind.
    LeastLoaded,
    /// Strict rotation. Cache-blind; present mainly as a baseline to measure against.
    RoundRobin,
    /// Fewest microseconds of recent mean latency, tie-broken by in-flight.
    ///
    /// For a pool whose members are *not* equivalent — a fast local GPU
    /// beside a slower one, or a hosted provider beside both — where
    /// least-loaded is misled because a slow backend with one request queued
    /// looks emptier than a fast one with two.
    ///
    /// Cache-blind, like the two above. On a pool of matched nodes serving
    /// long shared prefixes, `CacheAffinity` will beat this: a prefix cache
    /// hit is worth more than a few hundred microseconds of measured
    /// difference between identical machines.
    LowestLatency,
}

impl Policy {
    /// The spelling used on the wire, in the database and on the command
    /// line — one vocabulary, so a value typed into the UI, stored in a
    /// column and read from `--policy` cannot disagree.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CacheAffinity => "cache-affinity",
            Self::LeastLoaded => "least-loaded",
            Self::RoundRobin => "round-robin",
            Self::LowestLatency => "lowest-latency",
        }
    }

    /// Parse that spelling. `None` for anything else, which callers treat as
    /// "unset" rather than as an error: a control plane newer than this proxy
    /// may name a policy this build has never heard of, and falling back to
    /// the deployment default is better than refusing to route.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "cache-affinity" => Some(Self::CacheAffinity),
            "least-loaded" => Some(Self::LeastLoaded),
            "round-robin" => Some(Self::RoundRobin),
            "lowest-latency" => Some(Self::LowestLatency),
            _ => None,
        }
    }
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
        let mut candidates: Candidates = pool
            .iter()
            .filter(|b| b.is_healthy() && !exclude.contains(&b.uid))
            .collect();

        // Everything healthy has already failed — fall back to unhealthy
        // backends rather than returning an error. A stale health flag should
        // not turn a recoverable request into a 503.
        if candidates.is_empty() {
            candidates = pool.iter().filter(|b| !exclude.contains(&b.uid)).collect();
            if candidates.is_empty() {
                return None;
            }
        }

        if candidates.len() == 1 {
            return Some(Arc::clone(candidates[0]));
        }

        // The pool's own policy when it has one, the deployment's otherwise.
        // A model served by two identical local replicas and one fronted by
        // three hosted providers of different speeds want different answers,
        // and a single process routinely serves both.
        let policy = pool.policy.unwrap_or(self.policy);
        let (chosen, remember) = match policy {
            Policy::RoundRobin => {
                let n = self.rr.fetch_add(1, Ordering::Relaxed);
                (candidates[n % candidates.len()], false)
            }
            Policy::LeastLoaded => (least_loaded(&candidates, &self.rr), false),
            Policy::LowestLatency => (lowest_latency(&candidates, &self.rr), false),
            Policy::CacheAffinity => self.pick_affine(&candidates, prefix),
        };

        if remember {
            self.affinity.put(prefix, chosen.uid);
        }
        Some(Arc::clone(chosen))
    }

    /// Would [`pick`](Self::pick) still find a backend if these were excluded?
    ///
    /// Answers the retry question — "is there anywhere else to send this?" —
    /// without the side effects of actually picking (claiming a prefix,
    /// advancing the round-robin cursor). Mirrors `pick`'s last-resort rule
    /// that an unhealthy backend still counts as somewhere to go.
    pub fn has_candidate(&self, pool: &Pool, exclude: &[BackendUid]) -> bool {
        pool.iter().any(|b| !exclude.contains(&b.uid))
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
    pick_tied(candidates, prefix as usize)
}

/// Least loaded, with ties broken by rotation. Used by the cache-blind policy,
/// where there is no prefix to spread on.
fn least_loaded<'a>(candidates: &[&'a Arc<Backend>], rr: &AtomicUsize) -> &'a Arc<Backend> {
    pick_tied(candidates, rr.fetch_add(1, Ordering::Relaxed))
}

/// Lowest recent mean latency, with in-flight as the tie-break.
///
/// A backend that has completed nothing yet has no estimate, and is treated
/// as **eligible rather than fastest** — it goes into the tie-break group
/// alongside the current best. Ranking it as 0 µs would hand it the entire
/// pool until its first request finished; excluding it entirely would mean a
/// replacement backend never received a request at all, and so never earned
/// an estimate. Neither is a stable pool.
///
/// The tie-break is `least_loaded`, not rotation, because two backends of
/// equal measured speed should still be balanced by queue depth — and
/// because that is also what decides among several unmeasured ones.
///
/// Two scans over relaxed atomics, no allocation. Same cost shape as
/// `least_loaded`, which is the standard this path is held to.
fn lowest_latency<'a>(candidates: &[&'a Arc<Backend>], rr: &AtomicUsize) -> &'a Arc<Backend> {
    let best = candidates.iter().filter_map(|b| b.latency_us()).min();
    let Some(best) = best else {
        // Nothing measured anywhere: this is a cold pool, so fall back to the
        // policy that needs no history.
        return least_loaded(candidates, rr);
    };
    // Within 12.5% of the best counts as the same speed. Without a band the
    // pool oscillates: whichever backend last completed a fast request wins
    // every subsequent pick until its own queue slows it down.
    let ceiling = best + (best >> 3);
    let front: Vec<&Arc<Backend>> = candidates
        .iter()
        .filter(|b| b.latency_us().is_none_or(|us| us <= ceiling))
        .copied()
        .collect();
    if front.is_empty() {
        return least_loaded(candidates, rr);
    }
    least_loaded(&front, rr)
}

/// The `selector`-th of the least-loaded candidates.
///
/// Two scans over a handful of relaxed atomics and no allocation — this runs
/// per request. In-flight counts can move between the scans, so the tail
/// return covers the case where the minimum no longer matches anything; any
/// candidate is a defensible answer at that point.
fn pick_tied<'a>(candidates: &[&'a Arc<Backend>], selector: usize) -> &'a Arc<Backend> {
    let min = candidates.iter().map(|b| b.inflight()).min().unwrap_or(0);
    let tied = candidates.iter().filter(|b| b.inflight() == min).count();
    if tied > 0 {
        let mut nth = selector % tied;
        for b in candidates {
            if b.inflight() == min {
                if nth == 0 {
                    return b;
                }
                nth -= 1;
            }
        }
    }
    candidates[selector % candidates.len()]
}

/// Direct-mapped, lock-free prefix -> backend cache.
///
/// A full LRU is not worth it here: the access pattern is "a handful of hot
/// system prompts", collisions merely cost a cache miss, and every operation
/// has to sit on the per-request hot path. One relaxed atomic load and one
/// relaxed store beats any map with a lock in front of it.
///
/// Slot encoding: `(tag << 32) | uid`, where `tag` is the upper 32 bits of the
/// hash. Zero means empty, so the one hash that would encode to zero is simply
/// never cached. A 32-bit tag leaves a 2^-32 chance that a colliding prefix is
/// mistaken for a hit, which costs one misrouted request and no correctness.
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
        ((hash >> 32) << 32) | uid as u64
    }

    #[inline]
    fn get(&self, hash: u64) -> Option<BackendUid> {
        let slot = self.slots[hash as usize & self.mask].load(Ordering::Relaxed);
        if slot == 0 {
            return None;
        }
        // Confirm the slot belongs to this hash and not a colliding one.
        if slot >> 32 == hash >> 32 {
            Some(slot as BackendUid)
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

    /// The same pool, with a policy of its own.
    fn pool_with(policy: Policy) -> Pool {
        let base = two_node_pool();
        Arc::new(crate::registry::PoolInner {
            backends: base.backends.clone(),
            policy: Some(policy),
        })
    }

    /// A pool's own policy wins over the deployment's.
    ///
    /// The case this exists for: one control plane serving two identical
    /// local replicas *and* three hosted providers of different speeds. A
    /// single `--policy` has to be wrong for one of them.
    #[test]
    fn a_pool_with_its_own_policy_does_not_use_the_deployment_default() {
        // Deployment default is round-robin, which alternates.
        let r = router(Policy::RoundRobin);

        // With no policy of its own, the pool alternates.
        let shared = two_node_pool();
        let a = r.pick(&shared, 1, &[]).unwrap().uid;
        let b = r.pick(&shared, 1, &[]).unwrap().uid;
        assert_ne!(a, b, "round-robin alternates");

        // With lowest-latency of its own, the same pool sticks to the fast
        // backend no matter how many times it is asked.
        let own = pool_with(Policy::LowestLatency);
        for _ in 0..40 {
            own.backends[0].note_latency_us(2_000);
            own.backends[1].note_latency_us(60_000);
        }
        let fast = own.backends[0].uid;
        for _ in 0..20 {
            assert_eq!(
                r.pick(&own, 7, &[]).unwrap().uid,
                fast,
                "the pool's own policy decides, not the deployment's"
            );
        }
    }

    /// And an unset policy still means "whatever the deployment said", which
    /// is what every pool meant before this was settable.
    #[test]
    fn an_unset_pool_policy_falls_back_to_the_deployment() {
        let pool = two_node_pool();
        assert!(pool.policy.is_none());
        let r = router(Policy::CacheAffinity);
        // Cache affinity: the same prefix returns to the same backend.
        let first = r.pick(&pool, 42, &[]).unwrap().uid;
        for _ in 0..10 {
            assert_eq!(r.pick(&pool, 42, &[]).unwrap().uid, first);
        }
    }

    /// The whole point of the policy: a measurably slower backend stops
    /// receiving traffic, even though least-loaded would keep feeding it.
    #[test]
    fn lowest_latency_avoids_the_slow_backend() {
        let pool = two_node_pool();
        let r = router(Policy::LowestLatency);
        let backends: Vec<_> = pool.iter().collect();
        // One node is an order of magnitude slower. Enough samples that the
        // EWMA has converged, which is what the router actually reads.
        for _ in 0..40 {
            backends[0].note_latency_us(2_000);
            backends[1].note_latency_us(60_000);
        }
        let fast = backends[0].uid;

        let mut to_fast = 0;
        for i in 0..200 {
            if r.pick(&pool, i, &[]).unwrap().uid == fast {
                to_fast += 1;
            }
        }
        assert_eq!(to_fast, 200, "every request should go to the fast backend");
    }

    /// A backend that has completed nothing has no estimate, and must be
    /// *eligible* rather than either fastest or excluded.
    ///
    /// Ranking it 0 µs would hand it the whole pool before it had proven
    /// anything; excluding it would mean a freshly added backend never got a
    /// request, so never earned an estimate, so stayed excluded for ever.
    #[test]
    fn an_unmeasured_backend_is_eligible_but_not_automatically_fastest() {
        let pool = two_node_pool();
        let r = router(Policy::LowestLatency);
        let backends: Vec<_> = pool.iter().collect();
        for _ in 0..40 {
            backends[0].note_latency_us(5_000);
        }
        assert_eq!(backends[1].latency_us(), None, "second node is unmeasured");

        let mut to_new = 0;
        for i in 0..200 {
            if r.pick(&pool, i, &[]).unwrap().uid == backends[1].uid {
                to_new += 1;
            }
        }
        assert!(
            to_new > 0 && to_new < 200,
            "an unmeasured backend must get some traffic but not all of it: {to_new}/200"
        );
    }

    /// Backends of comparable speed are balanced by queue depth rather than
    /// having every request chase whichever one happened to finish quickest.
    #[test]
    fn comparable_latencies_still_spread() {
        let pool = two_node_pool();
        let r = router(Policy::LowestLatency);
        let backends: Vec<_> = pool.iter().collect();
        for _ in 0..40 {
            backends[0].note_latency_us(10_000);
            backends[1].note_latency_us(10_400); // within the 12.5% band
        }
        let mut seen = std::collections::HashSet::new();
        for i in 0..50 {
            seen.insert(r.pick(&pool, i, &[]).unwrap().uid);
        }
        assert_eq!(seen.len(), 2, "near-equal backends must both be used");
    }

    /// The EWMA must actually track a change, not average it away.
    #[test]
    fn the_latency_estimate_follows_a_backend_that_degrades() {
        let pool = two_node_pool();
        let b = pool.iter().next().unwrap();
        for _ in 0..50 {
            b.note_latency_us(1_000);
        }
        let settled = b.latency_us().unwrap();
        assert!((900..=1_100).contains(&settled), "settled at {settled}");

        for _ in 0..50 {
            b.note_latency_us(50_000);
        }
        let degraded = b.latency_us().unwrap();
        assert!(
            degraded > settled * 10,
            "a sustained slowdown must show: {settled} -> {degraded}"
        );
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
        let colliding = 0x1234_5678_9abc_def0u64 ^ (1 << 40);
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
