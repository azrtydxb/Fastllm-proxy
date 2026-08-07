//! Two-tier prompt classification for routing rules.
//!
//! A class is a name plus example prompts. The control plane embeds the
//! examples and averages them into a centroid; the data plane embeds the
//! request and takes the nearest centroid. There is no training step, no model
//! to fine-tune, and adding a class is a snapshot rebuild like adding a
//! backend.
//!
//! # Why two tiers
//!
//! Measured over ~21k real prompts (`bench/potion-real`, `bench/potion-wide`),
//! a static embedding — a token-vector lookup, no transformer — classifies in
//! **~140µs** and reaches 82-98% precision on classes that differ by *subject*:
//! coding, maths, chat, legal, finance, security, databases, devops. That is
//! most of what routing wants, and it is nearly free.
//!
//! It cannot separate classes that share a subject and differ by *intent* —
//! architecture versus coding sits at 48.7% precision among peers — because a
//! bag of token vectors has no word order, and "design a rate limiter" and
//! "debug my rate limiter" are the same bag. A contextual model recovers some
//! of that (65.9% among peers, 93.3% in isolation) for ~3.3ms, which is 24x
//! the cost.
//!
//! So: tier 1 on every request, tier 2 only where it changes an answer.
//!
//! # What makes the fast path stay fast
//!
//! Tier 2 is gated on *configuration*, not on a flag someone remembers to set.
//! [`Classifier::escalate_from`] is the set of tier-1 class names that some
//! **active** tier-2 class refines, computed at snapshot build. If no routing
//! rule references a tier-2 class, that set is empty, the transformer is never
//! loaded, and no request can pay for it. A deployment that only uses tier-1
//! classes is indistinguishable, at runtime, from one built before tier 2
//! existed.

use std::collections::HashSet;
use std::sync::Arc;

pub mod tier1;

#[cfg(feature = "classifier-tier2")]
pub mod tier2;

#[cfg(test)]
mod tests;

/// Which model decides a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// The static embedding, on every request.
    #[default]
    Fast,
    /// The transformer, only for requests the fast tier flagged as ambiguous.
    Refined,
}

impl Tier {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fast" => Some(Self::Fast),
            "refined" => Some(Self::Refined),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Refined => "refined",
        }
    }
}

/// One class, as the request path sees it: a name and a pre-averaged centroid.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptClass {
    pub name: String,
    pub tier: Tier,
    /// Normalised mean of this class's example embeddings, computed once at
    /// snapshot build. `None` when the class's examples could not be embedded
    /// (no model available at build time), which drops it from routing rather
    /// than letting it match nothing silently.
    pub centroid: Option<Vec<f32>>,
    /// How far ahead of the runner-up this class must be before a rule naming
    /// it will match.
    ///
    /// Per class rather than global because measured precision varies from 98%
    /// (coding) to 35% (extract) across classes an operator might define, and
    /// one threshold cannot serve both. Also per *model*: the two tiers have
    /// differently shaped spaces — bge-small packs everything into a narrow
    /// cone, so its margins run much smaller than the static model's for the
    /// same quality of separation. A floor copied between tiers is meaningless.
    pub min_margin: f32,
    /// Tier-1 class names this class competes with. Only consulted for
    /// [`Tier::Refined`] classes: when tier 1 answers one of these, the request
    /// is escalated so tier 2 can decide between them.
    pub refines: Vec<String>,
}

/// The classification a request received.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub class: String,
    pub margin: f32,
    /// Which tier decided. Reported so an operator can see how often the
    /// expensive path is actually taken.
    pub tier: Tier,
}

/// Everything routing needs to classify a prompt, built once per snapshot.
#[derive(Default)]
pub struct Classifier {
    classes: Vec<PromptClass>,
    /// Tier-1 class names that some active tier-2 class refines. Empty means
    /// tier 2 is never consulted, which is the common case and the one the
    /// fast path is optimised for.
    escalate_from: HashSet<String>,
}

impl Classifier {
    pub fn new(classes: Vec<PromptClass>) -> Self {
        // Only a tier-2 class that is actually present can cause an escalation,
        // and `refines` naming a class that does not exist is a no-op rather
        // than an error: the operator may have deleted the tier-1 class and not
        // yet tidied the reference, and refusing to route over that would turn
        // an untidy config into an outage.
        let known: HashSet<&str> = classes.iter().map(|c| c.name.as_str()).collect();
        let escalate_from = classes
            .iter()
            .filter(|c| c.tier == Tier::Refined && c.centroid.is_some())
            .flat_map(|c| c.refines.iter())
            .filter(|name| known.contains(name.as_str()))
            .cloned()
            .collect();
        Self {
            classes,
            escalate_from,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Whether any request could ever reach tier 2 under this configuration.
    ///
    /// The whole fast path rests on this being `false` for most deployments —
    /// see the module doc comment.
    pub fn tier2_reachable(&self) -> bool {
        !self.escalate_from.is_empty()
    }

    pub fn classes(&self) -> &[PromptClass] {
        &self.classes
    }

    /// Nearest centroid among the classes of one tier.
    ///
    /// Returns `None` when no class of that tier has a centroid, or when the
    /// winner does not clear its own `min_margin` — below the floor the caller
    /// is expected to fall through to the next routing rule, which is a routing
    /// decision rather than an error. Measured coverage at various floors is in
    /// `bench/potion-real`.
    fn nearest(
        &self,
        embedding: &[f32],
        tier: Tier,
        restrict: Option<&HashSet<&str>>,
    ) -> Option<Classification> {
        let mut best: Option<(&PromptClass, f32)> = None;
        let mut runner_up = f32::NEG_INFINITY;
        for class in self.classes.iter().filter(|c| c.tier == tier) {
            if restrict.is_some_and(|r| !r.contains(class.name.as_str())) {
                continue;
            }
            let Some(centroid) = &class.centroid else {
                continue;
            };
            let score = cosine(embedding, centroid);
            match best {
                Some((_, top)) if score <= top => runner_up = runner_up.max(score),
                Some((prev, top)) => {
                    runner_up = runner_up.max(top);
                    let _ = prev;
                    best = Some((class, score));
                }
                None => best = Some((class, score)),
            }
        }
        let (class, score) = best?;
        // A single class has nothing to be ahead of. Comparing against zero
        // rather than treating it as infinitely confident keeps the floor
        // meaningful — a lone class whose centroid the prompt barely resembles
        // should still fall through.
        let margin = if runner_up.is_finite() {
            score - runner_up
        } else {
            score
        };
        if margin < class.min_margin {
            return None;
        }
        Some(Classification {
            class: class.name.clone(),
            margin,
            tier,
        })
    }

    /// Classify a prompt, escalating only when configuration demands it.
    ///
    /// `embed_refined` is called at most once, and only when tier 1 landed on a
    /// class that an active tier-2 class refines. It is a closure rather than a
    /// borrowed model so the caller can keep the transformer behind a lazy
    /// initialiser that a tier-1-only deployment never runs.
    pub fn classify<F>(&self, fast_embedding: &[f32], embed_refined: F) -> Option<Classification>
    where
        F: FnOnce() -> Option<Vec<f32>>,
    {
        let first = self.nearest(fast_embedding, Tier::Fast, None)?;
        if !self.escalate_from.contains(&first.class) {
            return Some(first);
        }
        // Escalation: decide between the tier-2 classes that named this tier-1
        // class, and nothing else. A narrower question than the full taxonomy,
        // and measurably an easier one — architecture against coding alone
        // scores 93.3% where architecture among eleven domains scores 65.9%.
        let contenders: HashSet<&str> = self
            .classes
            .iter()
            .filter(|c| c.tier == Tier::Refined && c.refines.contains(&first.class))
            .map(|c| c.name.as_str())
            .collect();
        let Some(refined_embedding) = embed_refined() else {
            // The transformer is unavailable (not built in, or failed to load).
            // Tier 1's answer stands: a degraded routing decision beats
            // refusing the request over a classifier.
            return Some(first);
        };
        match self.nearest(&refined_embedding, Tier::Refined, Some(&contenders)) {
            Some(refined) => Some(refined),
            // Tier 2 declined to commit, so tier 1's answer stands. This is the
            // common shape: most "coding" prompts are coding, and only the
            // genuinely architectural ones clear the refined floor.
            None => Some(first),
        }
    }
}

/// Cosine similarity of two already-normalised vectors.
///
/// Both sides are normalised when they are built — centroids at snapshot build,
/// request embeddings in `tier1::embed` — so this is a plain dot product, which
/// is what keeps a classification to a few hundred nanoseconds on top of the
/// embedding itself.
#[inline]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::NEG_INFINITY;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Normalise in place. A zero vector is left alone rather than producing NaNs.
pub fn normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Normalised mean of a set of embeddings — how a class centroid is built.
pub fn centroid(vectors: &[Vec<f32>]) -> Option<Vec<f32>> {
    let first = vectors.first()?;
    let mut sum = vec![0.0f32; first.len()];
    for v in vectors {
        if v.len() != sum.len() {
            continue;
        }
        for (s, x) in sum.iter_mut().zip(v) {
            *s += x;
        }
    }
    for s in sum.iter_mut() {
        *s /= vectors.len() as f32;
    }
    normalise(&mut sum);
    Some(sum)
}

/// Shared handle, so a snapshot swap replaces the classifier without the
/// request path taking a lock.
pub type SharedClassifier = Arc<Classifier>;
