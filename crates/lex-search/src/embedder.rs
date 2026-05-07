//! Embedder trait: convert texts to fixed-dimension dense vectors.
//!
//! The trait is provider-agnostic so a future HTTP backend (Ollama,
//! OpenAI-compatible) plugs into the same call sites. Today only the
//! [`crate::MockEmbedder`] is implemented — see slice 2 in the issue
//! for the network-backed providers.

#[derive(Debug, Clone, thiserror::Error)]
pub enum EmbedError {
    #[error("embedder failure: {0}")]
    Generic(String),
}

/// A deterministic text → vector mapping. Implementations must
/// guarantee: same input → same output (within a process, and across
/// processes if the model is fixed). Length-zero input is allowed
/// and yields the zero vector.
pub trait Embedder: Send + Sync {
    /// Vector dimensionality. Must be constant for the lifetime of
    /// `self`.
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Output `Vec` has the same length as
    /// `texts`. Each inner `Vec` has length [`Self::dim`].
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Single-text convenience wrapper.
    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let mut out = self.embed_batch(&[text])?;
        out.pop().ok_or_else(|| EmbedError::Generic(
            "embed_batch returned empty result".into()))
    }
}
