//! Deterministic, no-network embedder for tests and offline use.
//!
//! Tokenises on whitespace + ASCII non-alphanumeric, lower-cases,
//! drops empties, then projects each token to a fixed coordinate via
//! SHA-256 mod `dim`. The vector is L2-normalised so cosine
//! similarity equals dot product.
//!
//! This is **not** semantically meaningful — "csv" and "comma-
//! separated-values" hash to unrelated coordinates — but it gives a
//! repeatable ordering for tests, and queries that share literal
//! tokens with a stage's description / signature / examples still
//! rank correctly.
//!
//! For real semantic search, swap in the HTTP embedder (slice 2).

use crate::embedder::{EmbedError, Embedder};
use sha2::{Digest, Sha256};

/// Default dimension for the mock embedder. Powers of two play
/// nicely with FFI but the value is otherwise arbitrary; kept small
/// so snapshot tests can compare full vectors without ceremony.
pub const MOCK_DIM: usize = 64;

#[derive(Debug, Clone)]
pub struct MockEmbedder {
    dim: usize,
}

impl Default for MockEmbedder {
    fn default() -> Self { Self { dim: MOCK_DIM } }
}

impl MockEmbedder {
    pub fn new() -> Self { Self::default() }

    pub fn with_dim(dim: usize) -> Self {
        assert!(dim > 0, "embedder dim must be positive");
        Self { dim }
    }
}

impl Embedder for MockEmbedder {
    fn dim(&self) -> usize { self.dim }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| embed_one(t, self.dim)).collect())
    }
}

fn embed_one(text: &str, dim: usize) -> Vec<f32> {
    let mut v = vec![0f32; dim];
    let lower = text.to_lowercase();
    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let mut h = Sha256::new();
        h.update(tok.as_bytes());
        let digest = h.finalize();
        // Use the first 4 bytes as a bucket index, the next 4 as a
        // signed weight. Spreads tokens across dimensions instead of
        // collapsing them to a one-hot.
        let bucket = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
            as usize % dim;
        let weight_bits = u32::from_le_bytes([digest[4], digest[5], digest[6], digest[7]]);
        let weight = (weight_bits as f32 / u32::MAX as f32) * 2.0 - 1.0;
        v[bucket] += weight;
    }
    l2_normalise(&mut v);
    v
}

fn l2_normalise(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() { *x /= norm; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_across_calls() {
        let e = MockEmbedder::new();
        let a = e.embed("parse csv into rows").unwrap();
        let b = e.embed("parse csv into rows").unwrap();
        assert_eq!(a, b, "same input must produce identical vectors");
    }

    #[test]
    fn different_text_produces_different_vector() {
        let e = MockEmbedder::new();
        let a = e.embed("parse csv").unwrap();
        let b = e.embed("send http post").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn empty_text_yields_zero_vector() {
        let e = MockEmbedder::new();
        let v = e.embed("").unwrap();
        assert_eq!(v.len(), MOCK_DIM);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn output_is_unit_length_when_nonzero() {
        let e = MockEmbedder::new();
        let v = e.embed("anything goes here").unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5,
            "non-empty embedding must be unit length, got norm {norm}");
    }

    #[test]
    fn batch_matches_per_item_calls() {
        let e = MockEmbedder::new();
        let inputs = ["a", "b c", "deeebeef"];
        let batched = e.embed_batch(&inputs).unwrap();
        for (i, txt) in inputs.iter().enumerate() {
            assert_eq!(batched[i], e.embed(txt).unwrap(),
                "batched embedding must match singular for `{txt}`");
        }
    }

    #[test]
    fn tokenisation_is_punctuation_insensitive() {
        let e = MockEmbedder::new();
        let a = e.embed("foo, bar.baz").unwrap();
        let b = e.embed("foo bar baz").unwrap();
        assert_eq!(a, b,
            "punctuation should not change the token bag");
    }

    #[test]
    fn tokenisation_is_case_insensitive() {
        let e = MockEmbedder::new();
        let a = e.embed("Hello World").unwrap();
        let b = e.embed("hello world").unwrap();
        assert_eq!(a, b);
    }
}
