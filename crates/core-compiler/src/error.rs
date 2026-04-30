//! Structured Core errors.

use crate::shape::ShapeError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CoreError {
    #[error("shape mismatch in `{op}` at {at}: {detail}")]
    ShapeMismatch {
        at: String,
        op: String,
        detail: String,
    },
    #[error("rank mismatch in `{op}` at {at}: expected {expected}, got {got}")]
    RankMismatch {
        at: String,
        op: String,
        expected: usize,
        got: usize,
    },
    #[error("`mut` binding `{name}` cannot escape stage `{stage}` at {at}")]
    MutEscape {
        at: String,
        stage: String,
        name: String,
    },
    #[error("unknown dtype `{dtype}` at {at}")]
    UnknownDtype {
        at: String,
        dtype: String,
    },
    /// A Lex-level type error surfaced via the Core wrapper.
    #[error("type error in core stage: {message}")]
    LexTypeError { message: String },
}

impl From<ShapeError> for CoreError {
    fn from(e: ShapeError) -> Self {
        match e {
            ShapeError::RankMismatch { a, b } => CoreError::RankMismatch {
                at: "?".into(),
                op: "?".into(),
                expected: a,
                got: b,
            },
            ShapeError::DimMismatch { a, b } => CoreError::ShapeMismatch {
                at: "?".into(),
                op: "?".into(),
                detail: format!("{a} ≠ {b}"),
            },
        }
    }
}
