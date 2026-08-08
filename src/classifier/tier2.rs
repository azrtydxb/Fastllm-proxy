//! Tier 2: the transformer, consulted only where tier 1 cannot decide.
//!
//! A real contextual model — 33M parameters, an ONNX forward pass. On the
//! deployed arm64 container that is **21-29 ms** per prompt against tier 1's
//! ~150 µs, measured with `classify-bench`; a laptop reads about 2 ms, and the
//! gap is the hardware rather than a misconfiguration. Worth that only for
//! classes that share a subject and differ by intent, which tier 1 structurally
//! cannot separate.
//!
//! It does not scale with cores: 3.5x the CPU quota bought 1.35x the speed
//! (28.7 ms on 2 cores, 21.3 ms on 7). Adding CPU is not the lever, which is
//! why `Options` exposes the two that measurably are, and why
//! `docs/classifier.md` records quantisation as the one that has not been tried.
//!
//! Loaded **lazily**, and warmed off the request path once configuration makes
//! escalation reachable (`AppState::warm_refined_tier`) — the load measures
//! ~410 ms in the container and used to be charged to whichever request was
//! first. A deployment that uses only tier-1 classes still never pays it.

use anyhow::{Context, Result};
use fastembed::{InitOptionsUserDefined, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel};
use parking_lot::Mutex;
use std::path::Path;

/// Matches tier 1's window closely enough that the two tiers see the same
/// prompt, which is what makes their margins comparable to each other's
/// history rather than only to their own.
pub const MAX_TOKENS: usize = 256;

/// Tuning that is worth measuring rather than guessing.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// ONNX Runtime intra-op threads. `None` leaves fastembed's default, which
    /// is `available_parallelism()`.
    ///
    /// Worth setting explicitly in a container: if that default reads the
    /// node's core count rather than the cgroup's quota, the runtime spawns
    /// more threads than the pod may run and they contend and get throttled.
    /// `classify-bench` measures both.
    pub intra_threads: Option<usize>,
    /// Token window. Cost is linear in it, and the window only has to be wide
    /// enough for the distinction being drawn.
    pub max_tokens: usize,
}

/// Intra-op threads beyond this buy nothing and eventually cost.
///
/// Measured with `classify-bench` in the deployed container (`docs/classifier.md`):
/// on a 7-core pod, 4 threads is the floor at 21.3 ms, where 8 threads is
/// 31.2 ms — half again as slow. On a 2-core pod the cap never binds.
const MAX_INTRA_THREADS: usize = 4;

impl Default for Options {
    fn default() -> Self {
        Self {
            intra_threads: Some(default_intra_threads()),
            max_tokens: MAX_TOKENS,
        }
    }
}

/// Threads to give ONNX Runtime, capped.
///
/// Set explicitly rather than left to fastembed's default for two measured
/// reasons. One thread is much worse than two — 50 ms against 29 ms on a
/// 2-core pod — so the floor matters. And more than four is worse again, so
/// the ceiling matters: fastembed's default is `available_parallelism()`, which
/// does read the cgroup quota on this kernel, but a host where it reported the
/// node's cores instead would land on the 8-thread configuration that measured
/// 1.8x slower than the best one.
fn default_intra_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, MAX_INTRA_THREADS)
}

pub struct Tier2 {
    /// `TextEmbedding::embed` takes `&mut self` (the ONNX session is not
    /// `Sync`), so a mutex is unavoidable without a second session.
    ///
    /// It does serialise escalations, and the critical section is 21-29 ms of
    /// CPU rather than the "few milliseconds" this comment used to claim:
    /// measured at four concurrent callers, per-prompt latency is unchanged
    /// from serial, so concurrency buys nothing and escalated throughput caps
    /// near 35/s per pod. A pool of sessions would lift that, but only where
    /// there are spare cores to run them — and the same measurement shows this
    /// model barely uses more than two.
    inner: Mutex<TextEmbedding>,
}

impl Tier2 {
    /// `dir` holds the ONNX weights and tokeniser, baked into the image.
    /// Deliberately not a HuggingFace repo id: this model is 130MB and a proxy
    /// must not reach the network to start serving.
    pub fn load(dir: &str) -> Result<Self> {
        Self::load_with(dir, Options::default())
    }

    pub fn load_with(dir: &str, options: Options) -> Result<Self> {
        let dir = Path::new(dir);
        let read = |name: &str| -> Result<Vec<u8>> {
            std::fs::read(dir.join(name))
                .with_context(|| format!("reading {name} from {}", dir.display()))
        };
        let files = TokenizerFiles {
            tokenizer_file: read("tokenizer.json")?,
            config_file: read("config.json")?,
            special_tokens_map_file: read("special_tokens_map.json")?,
            tokenizer_config_file: read("tokenizer_config.json")?,
        };
        let model = UserDefinedEmbeddingModel::new(read("model.onnx")?, files);
        let mut init = InitOptionsUserDefined::new().with_max_length(options.max_tokens);
        if let Some(threads) = options.intra_threads {
            init = init.with_intra_threads(threads);
        }
        let embedding = TextEmbedding::try_new_from_user_defined(model, init)
            .context("initialising the tier-2 classifier session")?;
        Ok(Self {
            inner: Mutex::new(embedding),
        })
    }

    /// Embed one prompt. Returns `None` on failure rather than propagating:
    /// the caller falls back to tier 1's answer, because a degraded routing
    /// decision beats refusing a request over a classifier.
    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let clipped: String = text.chars().take(2000).collect();
        let mut guard = self.inner.lock();
        let mut out = guard.embed(vec![clipped], Some(MAX_TOKENS)).ok()?;
        let mut v = out.pop()?;
        super::normalise(&mut v);
        Some(v)
    }

    pub fn embed_batch(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        let clipped: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(2000).collect())
            .collect();
        let mut guard = self.inner.lock();
        let mut vectors = guard.embed(clipped, Some(MAX_TOKENS)).ok()?;
        for v in vectors.iter_mut() {
            super::normalise(v);
        }
        Some(vectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ends of the measured curve. One thread costs 50 ms where two
    /// cost 29 on the same 2-core pod, and eight cost 53 where four cost 21 on
    /// a 7-core one — so the floor and the ceiling both matter, and a default
    /// that is simply `available_parallelism()` gets the ceiling wrong on any
    /// host where that reads the node rather than the cgroup.
    #[test]
    fn the_thread_default_stays_inside_the_measured_range() {
        let n = default_intra_threads();
        assert!(
            (2..=MAX_INTRA_THREADS).contains(&n),
            "{n} is outside the range the benchmark found usable"
        );
    }

    #[test]
    fn options_default_pins_a_thread_count_rather_than_deferring() {
        // Deferring is what fastembed does, and the point of this default is to
        // stop deferring — an unset value is the 8-thread configuration on the
        // wrong host.
        assert!(Options::default().intra_threads.is_some());
        assert_eq!(Options::default().max_tokens, MAX_TOKENS);
    }
}
