//! Structured type errors per spec §6.7.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypeError {
    TypeMismatch {
        at_node: String,
        expected: String,
        got: String,
        context: Vec<String>,
    },
    UnknownIdentifier {
        at_node: String,
        name: String,
    },
    ArityMismatch {
        at_node: String,
        expected: usize,
        got: usize,
    },
    NonExhaustiveMatch {
        at_node: String,
        missing: Vec<String>,
    },
    UnknownField {
        at_node: String,
        record_type: String,
        field: String,
    },
    DuplicateField {
        at_node: String,
        field: String,
    },
    UnknownVariant {
        at_node: String,
        constructor: String,
    },
    EffectNotDeclared {
        at_node: String,
        effect: String,
    },
    InfiniteType {
        at_node: String,
    },
    AmbiguousType {
        at_node: String,
    },
    RecursiveTypeWithoutConstructor {
        at_node: String,
        name: String,
    },
    /// Refinement-type predicate provably violated at a call site
    /// (#209 slice 2). The type checker statically discharged the
    /// refinement and found the literal argument doesn't satisfy the
    /// predicate. Slice 3 will add residual runtime checks for
    /// arguments that can't be discharged statically.
    RefinementViolation {
        at_node: String,
        fn_name: String,
        param_index: usize,
        binding: String,
        reason: String,
    },
}

impl TypeError {
    pub fn node(&self) -> &str {
        match self {
            TypeError::TypeMismatch { at_node, .. }
            | TypeError::UnknownIdentifier { at_node, .. }
            | TypeError::ArityMismatch { at_node, .. }
            | TypeError::NonExhaustiveMatch { at_node, .. }
            | TypeError::UnknownField { at_node, .. }
            | TypeError::DuplicateField { at_node, .. }
            | TypeError::UnknownVariant { at_node, .. }
            | TypeError::EffectNotDeclared { at_node, .. }
            | TypeError::InfiniteType { at_node, .. }
            | TypeError::AmbiguousType { at_node, .. }
            | TypeError::RecursiveTypeWithoutConstructor { at_node, .. }
            | TypeError::RefinementViolation { at_node, .. } => at_node,
        }
    }
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            TypeError::TypeMismatch { at_node, expected, got, context } => {
                write!(f, "type mismatch at {at_node}: expected {expected}, got {got}")?;
                if !context.is_empty() { write!(f, " ({})", context.join(" / "))?; }
                Ok(())
            }
            TypeError::UnknownIdentifier { at_node, name } => write!(f, "unknown identifier `{name}` at {at_node}"),
            TypeError::ArityMismatch { at_node, expected, got } => write!(f, "arity mismatch at {at_node}: expected {expected}, got {got}"),
            TypeError::NonExhaustiveMatch { at_node, missing } => write!(f, "non-exhaustive match at {at_node}: missing {missing:?}"),
            TypeError::UnknownField { at_node, record_type, field } => write!(f, "unknown field `{field}` on {record_type} at {at_node}"),
            TypeError::DuplicateField { at_node, field } => write!(f, "duplicate field `{field}` at {at_node}"),
            TypeError::UnknownVariant { at_node, constructor } => write!(f, "unknown constructor `{constructor}` at {at_node}"),
            TypeError::EffectNotDeclared { at_node, effect } => write!(f, "effect `{effect}` not declared at {at_node}"),
            TypeError::InfiniteType { at_node } => write!(f, "infinite type (occurs check) at {at_node}"),
            TypeError::AmbiguousType { at_node } => write!(f, "ambiguous type at {at_node}"),
            TypeError::RecursiveTypeWithoutConstructor { at_node, name } => write!(f, "recursive type {name} has no constructor at {at_node}"),
            TypeError::RefinementViolation { at_node, fn_name, param_index, binding, reason } =>
                write!(f, "refinement violated at {at_node}: argument {} of `{fn_name}` (binding `{binding}`): {reason}",
                    param_index + 1),
        }
    }
}

impl std::error::Error for TypeError {}
