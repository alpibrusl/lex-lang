//! Core type-checking pass. Reuses lex-types' HM checker for the Lex
//! subset, then runs Core extras: tensor shape check and (Phase 2)
//! mutation analysis.

use crate::error::CoreError;
use crate::shape::{ShapeExpr, ShapeSolver, Tensor};
use indexmap::IndexMap;
use lex_ast::Stage;

/// Phase-1 declaration of a Core stage. The body itself is parsed by the
/// Lex parser; this wrapper records the Core-specific signature info
/// (tensor types and dtypes) that's not yet expressible in the Lex
/// surface syntax.
#[derive(Debug, Clone)]
pub struct CoreStage {
    /// The Lex-side stage (function decl).
    pub stage: Stage,
    /// Type-level signature: each parameter's Core type.
    pub param_types: Vec<CoreType>,
    /// Return Core type.
    pub return_type: CoreType,
    /// Type parameters in order (for shape vars like M, K, N).
    #[allow(dead_code)]
    pub type_params: Vec<String>,
}

/// Core-flavored types. The `Lex` variant is the unchanged Lex `Ty`; new
/// variants encode tensor shapes and sized numerics.
#[derive(Debug, Clone, PartialEq)]
pub enum CoreType {
    Sized(SizedNumeric),
    Tensor(Tensor),
    /// Anything Lex's type system can express (Int, Str, records, ADTs).
    /// Encoded as a name plus optional type-arg list to keep this layer
    /// independent of `lex-types`.
    Lex { name: String, args: Vec<CoreType> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizedNumeric {
    U8, U16, U32, U64,
    I8, I16, I32, I64,
    F32, F64,
}

impl SizedNumeric {
    pub fn name(self) -> &'static str {
        use SizedNumeric::*;
        match self {
            U8 => "U8", U16 => "U16", U32 => "U32", U64 => "U64",
            I8 => "I8", I16 => "I16", I32 => "I32", I64 => "I64",
            F32 => "F32", F64 => "F64",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        use SizedNumeric::*;
        Some(match s {
            "U8" => U8, "U16" => U16, "U32" => U32, "U64" => U64,
            "I8" => I8, "I16" => I16, "I32" => I32, "I64" => I64,
            "F32" => F32, "F64" => F64,
            _ => return None,
        })
    }
    pub fn is_float(self) -> bool {
        matches!(self, SizedNumeric::F32 | SizedNumeric::F64)
    }
}

impl CoreType {
    pub fn pretty(&self) -> String {
        match self {
            CoreType::Sized(s) => s.name().into(),
            CoreType::Tensor(t) => t.pretty(),
            CoreType::Lex { name, args } if args.is_empty() => name.clone(),
            CoreType::Lex { name, args } => {
                let parts: Vec<String> = args.iter().map(|a| a.pretty()).collect();
                format!("{}[{}]", name, parts.join(", "))
            }
        }
    }
}

/// Run Core's checks on a declared stage. Returns the (refined)
/// `CoreStage` on success.
pub fn check_core_stage(stage: CoreStage) -> Result<CoreStage, Vec<CoreError>> {
    let mut errors = Vec::new();
    let mut solver = ShapeSolver::new();

    // Validate every parameter's tensor shape uses only known type params
    // or natural-number literals (deferred for Phase 2's free-var check).
    // For now, we ensure the dtype is a known Sized type.
    for (i, p) in stage.param_types.iter().enumerate() {
        if let CoreType::Tensor(t) = p {
            if SizedNumeric::parse(&t.dtype).is_none() {
                errors.push(CoreError::UnknownDtype {
                    at: format!("param {}", i + 1),
                    dtype: t.dtype.clone(),
                });
            }
        }
    }
    if let CoreType::Tensor(t) = &stage.return_type {
        if SizedNumeric::parse(&t.dtype).is_none() {
            errors.push(CoreError::UnknownDtype {
                at: "return type".into(),
                dtype: t.dtype.clone(),
            });
        }
    }

    // Detect call sites named `matmul` (or by convention) inside the body
    // and, given the static type signature, validate that the result
    // tensor's shape composes correctly. For Phase 1 we surface this as
    // a *signature-level* check: if the stage *declares* itself as a
    // matmul (matching the canonical (M,K) @ (K,N) -> (M,N) shape), we
    // verify the inner dim agrees.
    if let Some(err) = check_matmul_signature(&stage, &mut solver) {
        errors.push(err);
    }

    if errors.is_empty() { Ok(stage) } else { Err(errors) }
}

/// If the stage's signature looks like a matmul — exactly two tensor
/// parameters of rank 2 and a tensor return — verify shape composition.
fn check_matmul_signature(stage: &CoreStage, solver: &mut ShapeSolver) -> Option<CoreError> {
    if stage.param_types.len() != 2 { return None; }
    let (a, b) = match (&stage.param_types[0], &stage.param_types[1]) {
        (CoreType::Tensor(a), CoreType::Tensor(b)) => (a, b),
        _ => return None,
    };
    let r = match &stage.return_type {
        CoreType::Tensor(r) => r,
        _ => return None,
    };
    if a.shape.len() != 2 || b.shape.len() != 2 || r.shape.len() != 2 {
        return None;
    }
    let stage_name = match &stage.stage {
        Stage::FnDecl(fd) => fd.name.clone(),
        _ => "?".into(),
    };
    // a: [M, K], b: [K, N], r: [M, N]
    let a_inner = &a.shape[1];
    let b_outer = &b.shape[0];
    if let Err(e) = solver.unify(a_inner, b_outer) {
        return Some(CoreError::ShapeMismatch {
            at: format!("function `{stage_name}`"),
            op: "matmul-shape".into(),
            detail: format!(
                "inner dim {} of arg 1 doesn't match outer dim {} of arg 2 ({e})",
                a_inner.pretty(), b_outer.pretty(),
            ),
        });
    }
    // Check return shape M, N agrees with a/b.
    if let Err(e) = solver.unify(&a.shape[0], &r.shape[0]) {
        return Some(CoreError::ShapeMismatch {
            at: format!("function `{stage_name}`"),
            op: "matmul-shape".into(),
            detail: format!("M dim mismatch: arg 1 says {}, return says {} ({e})",
                a.shape[0].pretty(), r.shape[0].pretty()),
        });
    }
    if let Err(e) = solver.unify(&b.shape[1], &r.shape[1]) {
        return Some(CoreError::ShapeMismatch {
            at: format!("function `{stage_name}`"),
            op: "matmul-shape".into(),
            detail: format!("N dim mismatch: arg 2 says {}, return says {} ({e})",
                b.shape[1].pretty(), r.shape[1].pretty()),
        });
    }
    None
}

/// Helper: build a `Tensor` quickly for tests.
pub fn matrix(m: ShapeExpr, n: ShapeExpr, dtype: &str) -> Tensor {
    Tensor { shape: vec![m, n], dtype: dtype.into() }
}

/// Helper: a stage map (reserved for Phase 2 use).
#[allow(dead_code)]
pub(crate) type Bindings = IndexMap<String, CoreType>;
