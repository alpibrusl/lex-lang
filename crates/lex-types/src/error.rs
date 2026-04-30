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
            | TypeError::RecursiveTypeWithoutConstructor { at_node, .. } => at_node,
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
        }
    }
}

impl std::error::Error for TypeError {}
