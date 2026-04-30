//! Tensor shape language and solver. Spec §13.4.
//!
//! Shapes are sequences of `ShapeExpr`s: nat literals, type variables, or
//! arithmetic over them (`M+1`, `2*N`). The solver normalizes constants
//! and unifies variables, returning a structured error when two shape
//! expressions can't be reconciled.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShapeExpr {
    /// A natural-number literal.
    Lit { value: i64 },
    /// A type variable (e.g. `M` or `N` in a `matmul[M, K, N](...)` signature).
    Var { name: String },
    Add { lhs: Box<ShapeExpr>, rhs: Box<ShapeExpr> },
    Mul { lhs: Box<ShapeExpr>, rhs: Box<ShapeExpr> },
}

impl ShapeExpr {
    pub fn lit(n: i64) -> Self { ShapeExpr::Lit { value: n } }
    pub fn var(name: impl Into<String>) -> Self { ShapeExpr::Var { name: name.into() } }
    pub fn sum(a: ShapeExpr, b: ShapeExpr) -> Self {
        ShapeExpr::Add { lhs: Box::new(a), rhs: Box::new(b) }
    }
    pub fn product(a: ShapeExpr, b: ShapeExpr) -> Self {
        ShapeExpr::Mul { lhs: Box::new(a), rhs: Box::new(b) }
    }

    /// Display form: "M", "1024", "(M+1)", "(2*N)".
    pub fn pretty(&self) -> String {
        match self {
            ShapeExpr::Lit { value } => value.to_string(),
            ShapeExpr::Var { name } => name.clone(),
            ShapeExpr::Add { lhs, rhs } => format!("({}+{})", lhs.pretty(), rhs.pretty()),
            ShapeExpr::Mul { lhs, rhs } => format!("({}*{})", lhs.pretty(), rhs.pretty()),
        }
    }
}

/// `Tensor[shape, dtype]`. `dtype` is a primitive numeric kind name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tensor {
    pub shape: Vec<ShapeExpr>,
    pub dtype: String,
}

impl Tensor {
    pub fn pretty(&self) -> String {
        let dims: Vec<String> = self.shape.iter().map(|e| e.pretty()).collect();
        format!("Tensor[[{}], {}]", dims.join(", "), self.dtype)
    }
}

/// Constraint solver: equate shape expressions to bind variables and
/// detect mismatches.
#[derive(Default)]
pub struct ShapeSolver {
    /// Known bindings: var name → simplified expression.
    bindings: IndexMap<String, ShapeExpr>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ShapeError {
    #[error("rank mismatch: {a} dimensions vs {b}")]
    RankMismatch { a: usize, b: usize },
    #[error("dimension mismatch: {a} ≠ {b}")]
    DimMismatch { a: String, b: String },
}

impl ShapeSolver {
    pub fn new() -> Self { Self::default() }

    /// Resolve a shape expression by substituting any known variable
    /// bindings, then folding constants.
    pub fn resolve(&self, e: &ShapeExpr) -> ShapeExpr {
        match e {
            ShapeExpr::Lit { .. } => e.clone(),
            ShapeExpr::Var { name } => match self.bindings.get(name) {
                Some(bound) => self.resolve(bound),
                None => e.clone(),
            },
            ShapeExpr::Add { lhs, rhs } => {
                let a = self.resolve(lhs);
                let b = self.resolve(rhs);
                if let (ShapeExpr::Lit { value: x }, ShapeExpr::Lit { value: y }) = (&a, &b) {
                    ShapeExpr::Lit { value: x + y }
                } else {
                    ShapeExpr::sum(a, b)
                }
            }
            ShapeExpr::Mul { lhs, rhs } => {
                let a = self.resolve(lhs);
                let b = self.resolve(rhs);
                if let (ShapeExpr::Lit { value: x }, ShapeExpr::Lit { value: y }) = (&a, &b) {
                    ShapeExpr::Lit { value: x * y }
                } else {
                    ShapeExpr::product(a, b)
                }
            }
        }
    }

    /// Unify two shape expressions, recording bindings as needed.
    pub fn unify(&mut self, a: &ShapeExpr, b: &ShapeExpr) -> Result<(), ShapeError> {
        let a = self.resolve(a);
        let b = self.resolve(b);
        match (&a, &b) {
            (ShapeExpr::Lit { value: x }, ShapeExpr::Lit { value: y }) => {
                if x == y { Ok(()) }
                else { Err(ShapeError::DimMismatch { a: a.pretty(), b: b.pretty() }) }
            }
            (ShapeExpr::Var { name }, other) | (other, ShapeExpr::Var { name }) => {
                self.bindings.insert(name.clone(), other.clone());
                Ok(())
            }
            // Structural arithmetic terms: only equal if both reduce to the
            // same canonical form. We don't do full algebraic unification;
            // require them to match after `resolve`.
            _ if a == b => Ok(()),
            _ => Err(ShapeError::DimMismatch { a: a.pretty(), b: b.pretty() }),
        }
    }

    /// Unify two whole shapes element-wise.
    pub fn unify_shapes(&mut self, a: &[ShapeExpr], b: &[ShapeExpr]) -> Result<(), ShapeError> {
        if a.len() != b.len() {
            return Err(ShapeError::RankMismatch { a: a.len(), b: b.len() });
        }
        for (x, y) in a.iter().zip(b) { self.unify(x, y)?; }
        Ok(())
    }
}
