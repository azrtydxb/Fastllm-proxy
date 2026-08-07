//! Tier 1: the static embedding that runs on every classified request.
//!
//! Model2Vec is a token-vector lookup table plus a mean — no transformer
//! forward pass, no matmul, no ONNX runtime. That is what buys ~140µs and what
//! limits it to distinctions visible in vocabulary alone.
//!
//! Loading is fallible and non-fatal by design: a proxy that cannot load the
//! model still serves every request, it just cannot match a rule that names a
//! class. Refusing to start would turn a classifier problem into an outage.

use anyhow::{Context, Result};
use model2vec_rs::model::StaticModel;
use std::path::Path;

/// How much of a prompt the classification looks at, in tokens.
///
/// 128 measured better than 32 on real prompts (98.7% against 98.2% for
/// coding) because a coding question's giveaway is often the pasted code below
/// the first line rather than the first line itself. It also bounds the cost:
/// the encoder stops there, so a 64KB paste costs exactly what a 4KB one does.
pub const MAX_TOKENS: usize = 128;

pub struct Tier1 {
    model: StaticModel,
}

impl Tier1 {
    /// `source` is a local directory or a HuggingFace repo id. A directory is
    /// what a container should use — see the Dockerfile, which bakes the model
    /// in so startup does no network I/O.
    pub fn load(source: &str) -> Result<Self> {
        let path = Path::new(source);
        let model = StaticModel::from_pretrained(source, None, None, None).with_context(|| {
            if path.exists() {
                format!("loading the tier-1 classifier model from {source}")
            } else {
                format!(
                    "loading the tier-1 classifier model {source:?}: not a local path, so this \
                     was attempted as a HuggingFace repo id and needs network access. Bake the \
                     model into the image and point --classifier-model at it."
                )
            }
        })?;
        Ok(Self { model })
    }

    /// Embed one prompt, normalised so the comparison is a dot product.
    ///
    /// Goes through the batch entry point with a single element: `encode_single`
    /// exists but takes no token cap, and the cap is what bounds the cost of a
    /// large pasted prompt — a 64KB paste has to cost what a 4KB one does.
    pub fn embed(&self, text: &str) -> Vec<f32> {
        let mut out = self.model.encode_with_args(
            std::slice::from_ref(&text.to_string()),
            Some(MAX_TOKENS),
            1,
        );
        let mut v = out.pop().unwrap_or_default();
        super::normalise(&mut v);
        v
    }

    /// Embed many, for building centroids at snapshot time.
    pub fn embed_batch(&self, texts: &[String]) -> Vec<Vec<f32>> {
        let mut vectors = self.model.encode_with_args(texts, Some(MAX_TOKENS), 1024);
        for v in vectors.iter_mut() {
            super::normalise(v);
        }
        vectors
    }
}
