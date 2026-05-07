//! Cosine similarity + three-component fusion.
//!
//! Vectors are assumed L2-normalised by the embedder, so cosine
//! reduces to a dot product. We still fall back to a divisor when a
//! vector slips through unnormalised — defensive arithmetic is
//! cheap; debugging a NaN search ranking from a bad embedder result
//! is not.

use crate::{ScoreBreakdown, W_DESCRIPTION, W_EXAMPLES, W_SIGNATURE};

/// Cosine similarity in `[-1.0, 1.0]`. Returns 0 for any vector pair
/// where either norm is zero or the lengths disagree (that's an
/// embedder bug — caller-visible failure would just be louder noise).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(f32::EPSILON);
    dot / denom
}

/// Combine the three component scores into a single ranking score.
///
/// `description` and `examples` are optional because not every stage
/// has a `note` or attached tests. When `description` is absent its
/// 0.5 weight folds into the signature, ensuring a well-typed but
/// undocumented function still ranks against a query rather than
/// silently scoring half what it should.
///
/// The `examples` weight does **not** redistribute when absent — a
/// stage without examples isn't penalised, it just contributes the
/// effective max of `(W_DESCRIPTION+W_SIGNATURE)` instead of `1.0`.
/// This keeps the absolute score interpretable: "X% of theoretical
/// max for this stage shape".
pub fn fuse_scores(
    description: Option<f32>,
    signature: f32,
    examples: Option<f32>,
) -> ScoreBreakdown {
    let (desc_w, sig_w) = match description {
        Some(_) => (W_DESCRIPTION, W_SIGNATURE),
        None => (0.0, W_DESCRIPTION + W_SIGNATURE),
    };
    let ex_w = if examples.is_some() { W_EXAMPLES } else { 0.0 };

    let fused = description.unwrap_or(0.0) * desc_w
        + signature * sig_w
        + examples.unwrap_or(0.0) * ex_w;

    ScoreBreakdown {
        description,
        signature,
        examples,
        fused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn cosine_of_identical_unit_vectors_is_one() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!(approx(cosine_similarity(&v, &v), 1.0));
    }

    #[test]
    fn cosine_of_orthogonal_unit_vectors_is_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(approx(cosine_similarity(&a, &b), 0.0));
    }

    #[test]
    fn cosine_of_opposite_vectors_is_negative_one() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![-1.0_f32, 0.0];
        assert!(approx(cosine_similarity(&a, &b), -1.0));
    }

    #[test]
    fn cosine_handles_unnormalised_inputs() {
        let a = vec![3.0_f32, 0.0];
        let b = vec![5.0_f32, 0.0];
        assert!(approx(cosine_similarity(&a, &b), 1.0),
            "cosine should be magnitude-invariant");
    }

    #[test]
    fn cosine_handles_dim_mismatch_safely() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![1.0_f32];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_of_zero_vector_is_zero_not_nan() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0];
        let c = cosine_similarity(&a, &b);
        assert!(!c.is_nan());
        assert!(approx(c, 0.0));
    }

    #[test]
    fn fusion_uses_stated_weights_when_all_components_present() {
        let b = fuse_scores(Some(1.0), 1.0, Some(1.0));
        assert!(approx(b.fused, W_DESCRIPTION + W_SIGNATURE + W_EXAMPLES));
    }

    #[test]
    fn fusion_redistributes_description_weight_when_absent() {
        // No description: 0.5 weight folds into signature, examples
        // stays at 0.2. So perfect match on sig+examples should give
        // (0.5 + 0.3) * 1.0 + 0.2 * 1.0 == 1.0.
        let b = fuse_scores(None, 1.0, Some(1.0));
        assert!(approx(b.fused, 1.0),
            "expected 1.0 with redistribution, got {}", b.fused);
    }

    #[test]
    fn fusion_does_not_redistribute_examples_weight() {
        // Examples missing: stage was good but had no input/output
        // pairs attached. Score caps at 0.5 + 0.3 = 0.8.
        let b = fuse_scores(Some(1.0), 1.0, None);
        assert!(approx(b.fused, W_DESCRIPTION + W_SIGNATURE),
            "expected 0.8 with no examples, got {}", b.fused);
    }

    #[test]
    fn fusion_caps_at_signature_only() {
        // Neither description nor examples → all weight on signature.
        let b = fuse_scores(None, 1.0, None);
        assert!(approx(b.fused, W_DESCRIPTION + W_SIGNATURE),
            "expected 0.8 (description fold-in), got {}", b.fused);
    }

    #[test]
    fn fusion_breakdown_preserves_inputs() {
        let b = fuse_scores(Some(0.7), 0.9, Some(0.4));
        assert_eq!(b.description, Some(0.7));
        assert_eq!(b.signature, 0.9);
        assert_eq!(b.examples, Some(0.4));
    }
}
