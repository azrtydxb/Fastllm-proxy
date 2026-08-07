//! Vector arithmetic shared by the classifier and the control plane.
//!
//! Unconditional, not behind the `classifier` feature: the control plane's
//! admin API reports on centroids the snapshot carries even in a build with no
//! embedding model at all, and `control::build` averages example embeddings
//! without knowing what produced them. Keeping these three functions in one
//! place is what stops a third copy of `cosine` appearing the next time
//! something needs one — there were already two.

/// Scale to unit length in place. A zero vector is left alone rather than
/// producing NaNs.
pub fn normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity of two already-normalised vectors — a plain dot product.
///
/// Mismatched dimensions return negative infinity rather than a partial sum, so
/// a centroid built by one model can never out-score one built by another
/// simply by being shorter. That is not hypothetical: the two classifier tiers
/// have different dimensionalities, and a configuration error that mixed them
/// would otherwise rank silently.
#[inline]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return f32::NEG_INFINITY;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Normalised mean of a set of embeddings — how a class centroid is built.
///
/// Vectors of the wrong length are skipped rather than truncated: a short
/// vector added component-wise would silently drag the centroid toward the
/// origin in every dimension it lacks.
pub fn centroid(vectors: &[Vec<f32>]) -> Option<Vec<f32>> {
    let first = vectors.first()?;
    let mut sum = vec![0.0f32; first.len()];
    let mut counted = 0usize;
    for v in vectors {
        if v.len() != sum.len() {
            continue;
        }
        counted += 1;
        for (s, x) in sum.iter_mut().zip(v) {
            *s += x;
        }
    }
    if counted == 0 {
        return None;
    }
    for s in sum.iter_mut() {
        *s /= counted as f32;
    }
    normalise(&mut sum);
    Some(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_of_mismatched_dimensions_never_wins() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), f32::NEG_INFINITY);
    }

    #[test]
    fn centroid_is_unit_length_and_averages_evenly() {
        let c = centroid(&[vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]).unwrap();
        let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!((c[0] - c[1]).abs() < 1e-6);
        assert_eq!(c[2], 0.0);
    }

    #[test]
    fn a_wrong_length_vector_is_skipped_not_truncated() {
        let c = centroid(&[vec![1.0, 0.0], vec![9.0, 9.0, 9.0]]).unwrap();
        assert!(
            (c[0] - 1.0).abs() < 1e-6,
            "the odd one out contributed nothing"
        );
    }

    #[test]
    fn an_empty_set_has_no_centroid() {
        assert!(centroid(&[]).is_none());
    }
}
