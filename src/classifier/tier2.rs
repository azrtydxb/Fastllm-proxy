//! Tier 2: the transformer, consulted only where tier 1 cannot decide.
//!
//! A real contextual model — 33M parameters, an ONNX forward pass — at ~3.3ms
//! against tier 1's ~140µs. Worth 24x the cost only for classes that share a
//! subject and differ by intent, which tier 1 structurally cannot separate.
//!
//! Loaded **lazily**: `Tier2::get` is only ever called when the snapshot
//! contains an active tier-2 class, so a deployment that uses only tier-1
//! classes never pays the ~85ms initialisation, never maps the weights, and
//! never links the runtime at run time.

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

impl Default for Options {
    fn default() -> Self {
        Self {
            intra_threads: None,
            max_tokens: MAX_TOKENS,
        }
    }
}

pub struct Tier2 {
    /// `TextEmbedding::embed` takes `&mut self` (the ONNX session is not
    /// `Sync`), so a mutex is unavoidable. It is uncontended in practice:
    /// escalation is a small fraction of a small fraction of traffic, and the
    /// critical section is a few milliseconds of CPU with no I/O in it.
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
