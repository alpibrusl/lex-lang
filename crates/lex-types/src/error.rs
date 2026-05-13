//! Structured type errors per spec §6.7.

use crate::position::Position;
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
    /// A function carrying signature-level examples (#369) declares
    /// at least one effect. v1 restricts examples to pure functions so
    /// the "same inputs ⇒ same outputs" invariant (rule #5) holds
    /// without modeling effect responses.
    ExamplesOnEffectfulFn {
        at_node: String,
        fn_name: String,
    },
    /// A signature-level example case (#369) supplies the wrong number
    /// of arguments for the function it documents.
    ExampleArityMismatch {
        at_node: String,
        fn_name: String,
        /// Zero-based index of the failing case in the `examples` block.
        case_index: usize,
        expected: usize,
        got: usize,
    },
    /// A signature-level example case (#369 slice 2) ran successfully
    /// but the function's actual output disagrees with the declared
    /// `expected` value. This is the load-bearing check that makes the
    /// `examples` block enforce behavior, not just types.
    ExampleMismatch {
        at_node: String,
        fn_name: String,
        case_index: usize,
        /// Pretty-printed expected value (LHS of the `=>` in the example).
        expected: String,
        /// Pretty-printed actual value the function body produced.
        got: String,
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
            | TypeError::RefinementViolation { at_node, .. }
            | TypeError::ExamplesOnEffectfulFn { at_node, .. }
            | TypeError::ExampleArityMismatch { at_node, .. }
            | TypeError::ExampleMismatch { at_node, .. } => at_node,
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
            TypeError::ExamplesOnEffectfulFn { at_node, fn_name } =>
                write!(f, "function `{fn_name}` at {at_node} carries `examples` but declares effects; \
                          v1 restricts examples to pure functions"),
            TypeError::ExampleArityMismatch { at_node, fn_name, case_index, expected, got } =>
                write!(f, "example #{} of `{fn_name}` at {at_node}: expected {expected} argument(s), got {got}",
                    case_index + 1),
            TypeError::ExampleMismatch { at_node, fn_name, case_index, expected, got } =>
                write!(f, "example #{} of `{fn_name}` at {at_node}: expected {expected}, got {got}",
                    case_index + 1),
        }
    }
}

impl std::error::Error for TypeError {}

/// `TypeError` enriched with an optional source `Position` (#306
/// slice 1) plus a `rule_tag` + `rule_explanation` (#306 slice 2).
/// `lex_types::check_program_with_positions` returns a
/// `Vec<PositionedError>`; the bare `check_program` keeps the old
/// `Vec<TypeError>` shape for backwards compatibility.
///
/// Serializes as a flat JSON object: the wrapped error's fields
/// (`kind`, `at_node`, `expected`, …), the derived `rule_tag` +
/// `rule_explanation`, and a `position` field when one was attached.
/// Consumers can downcast via the `error` field or pattern-match on
/// the `kind` tag in the JSON. The `rule_tag` is the stable
/// kebab-case identifier LLM prompts should reference; the
/// `rule_explanation` is a plain-language description of what the
/// rule enforces, suitable to inline in a repair prompt.
#[derive(Debug, Clone)]
pub struct PositionedError {
    pub error: TypeError,
    pub position: Option<Position>,
}

impl Serialize for PositionedError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        // Serialize the inner TypeError to a JSON map, then add the
        // rule_tag/rule_explanation/position fields. This keeps the
        // tagged-enum encoding (kind + fields) intact while flattening
        // the extra computed fields on top.
        let inner = serde_json::to_value(&self.error)
            .map_err(serde::ser::Error::custom)?;
        let obj = inner
            .as_object()
            .ok_or_else(|| serde::ser::Error::custom("TypeError did not serialize as an object"))?;
        let extra = if self.position.is_some() { 3 } else { 2 };
        let mut map = serializer.serialize_map(Some(obj.len() + extra))?;
        for (k, v) in obj {
            map.serialize_entry(k, v)?;
        }
        map.serialize_entry("rule_tag", self.error.rule_tag())?;
        map.serialize_entry("rule_explanation", self.error.rule_explanation())?;
        if let Some(p) = &self.position {
            map.serialize_entry("position", p)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for PositionedError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Round-trip through a serde_json::Value so the embedded
        // TypeError's tagged-enum encoding parses correctly even
        // when the rule_tag/rule_explanation/position siblings are
        // present.
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let position = value
            .as_object_mut()
            .and_then(|o| o.remove("position"))
            .and_then(|p| serde_json::from_value::<Position>(p).ok());
        if let Some(o) = value.as_object_mut() {
            o.remove("rule_tag");
            o.remove("rule_explanation");
        }
        let error: TypeError =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(PositionedError { error, position })
    }
}

impl PositionedError {
    pub fn new(error: TypeError, position: Option<Position>) -> Self {
        Self { error, position }
    }

    pub fn without_position(error: TypeError) -> Self {
        Self { error, position: None }
    }

    pub fn node(&self) -> &str {
        self.error.node()
    }
}

impl std::fmt::Display for PositionedError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self.position {
            Some(p) => write!(f, "[{}] {}", p.render(), self.error),
            None => self.error.fmt(f),
        }
    }
}

impl std::error::Error for PositionedError {}

impl From<TypeError> for PositionedError {
    fn from(e: TypeError) -> Self {
        Self::without_position(e)
    }
}
