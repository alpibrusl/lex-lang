//! Rule-tagged messages for type errors (#306 slice 2).
//!
//! Every `TypeError` variant maps to a stable `rule_tag` (a kebab-
//! case identifier) and a `rule_explanation` (plain-language
//! description of what the rule enforces). LLM repair flows that
//! reference the `rule_tag` get measurably better repair attempts
//! because the model can cross-reference the rule across many
//! prior examples.
//!
//! The tag is stable across releases — once shipped, a rule_tag
//! never changes meaning. New rules get new tags; existing
//! variants that split into more specific sub-rules will be
//! handled by adding new sibling tags, not by repurposing existing
//! ones.

use crate::error::TypeError;

/// Catalog entry for one rule.
#[derive(Debug, Clone, Copy)]
pub struct RuleInfo {
    pub tag: &'static str,
    pub explanation: &'static str,
}

impl TypeError {
    /// Stable kebab-case identifier for this error variant. See
    /// [`all_rules`] for the full catalog.
    pub fn rule_tag(&self) -> &'static str {
        match self {
            TypeError::TypeMismatch { .. } => "type-mismatch",
            TypeError::UnknownIdentifier { .. } => "unknown-identifier",
            TypeError::ArityMismatch { .. } => "arity-mismatch",
            TypeError::NonExhaustiveMatch { .. } => "non-exhaustive-match",
            TypeError::UnknownField { .. } => "unknown-field",
            TypeError::DuplicateField { .. } => "duplicate-field",
            TypeError::UnknownVariant { .. } => "unknown-variant",
            TypeError::EffectNotDeclared { .. } => "effect-not-declared",
            TypeError::InfiniteType { .. } => "infinite-type",
            TypeError::AmbiguousType { .. } => "ambiguous-type",
            TypeError::RecursiveTypeWithoutConstructor { .. } => "recursive-type-without-constructor",
            TypeError::RefinementViolation { .. } => "refinement-violation",
        }
    }

    /// Plain-language description of what the rule enforces. Aimed
    /// at LLM repair-flow prompts: short enough to inline in a
    /// system message, specific enough to suggest the next move.
    pub fn rule_explanation(&self) -> &'static str {
        explanation_for_tag(self.rule_tag())
    }
}

fn explanation_for_tag(tag: &str) -> &'static str {
    match tag {
        "type-mismatch" => "An expression's inferred type doesn't match what the surrounding context requires \
(return type, let-binding annotation, function argument, operator operand, etc.). Fix by changing \
the expression to produce the expected type, or by adjusting the declared/inferred expected type \
to match.",
        "unknown-identifier" => "A name referenced in scope is not declared. Either the binding is missing, \
the name is misspelled, or an `import` is missing. Check for typos first; then verify the relevant \
`let`, parameter, or top-level `fn` is in scope.",
        "arity-mismatch" => "A call site supplies a different number of arguments than the function or \
constructor accepts. Either add the missing arguments or remove the extras.",
        "non-exhaustive-match" => "A `match` expression doesn't cover every case of its scrutinee's type. \
Add the missing arms listed in the error, or add a `_` wildcard if catching the remainder is intended.",
        "unknown-field" => "A record field access or literal references a field name that isn't part of \
the record type. Verify spelling and that the type really has that field — check the type declaration.",
        "duplicate-field" => "A record literal lists the same field name twice. Each field must appear \
exactly once. Remove the duplicate or rename one of them.",
        "unknown-variant" => "A constructor pattern or expression references a variant name that isn't \
part of the union type. Verify spelling and that the variant exists on this union.",
        "effect-not-declared" => "A function body invokes an effect (io, fs_read, net, …) that the \
function's signature doesn't declare. Either add the effect to the function's `[effects]` annotation \
or remove the call that produces it.",
        "infinite-type" => "Inference would require a type to contain itself (e.g. `t = List<t>` with no \
constructor). Add a nominal type wrapper or restructure the data so the recursion is mediated by a \
named type.",
        "ambiguous-type" => "Inference couldn't pick a single concrete type for an expression. Add a type \
annotation to disambiguate.",
        "recursive-type-without-constructor" => "A type alias references itself with no constructor in \
between, so no value of the type can ever be built. Make the recursive position carry a constructor \
(e.g. `Cons<T, List<T>> | Nil`).",
        "refinement-violation" => "A literal argument provably violates a refinement-type predicate \
(#209). Adjust the argument to satisfy the predicate, or relax the predicate at the function \
signature.",
        _ => "Unknown rule. The rule_tag may have been introduced after this Lex release.",
    }
}

/// The full rule catalog, in stable order. Used by `lex docs --rules`
/// and by tooling that wants to enumerate every supported rule
/// (e.g. an LSP server building a code-actions registry).
pub fn all_rules() -> &'static [RuleInfo] {
    &[
        RuleInfo { tag: "type-mismatch", explanation: TYPE_MISMATCH },
        RuleInfo { tag: "unknown-identifier", explanation: UNKNOWN_IDENT },
        RuleInfo { tag: "arity-mismatch", explanation: ARITY_MISMATCH },
        RuleInfo { tag: "non-exhaustive-match", explanation: NON_EXHAUSTIVE },
        RuleInfo { tag: "unknown-field", explanation: UNKNOWN_FIELD },
        RuleInfo { tag: "duplicate-field", explanation: DUPLICATE_FIELD },
        RuleInfo { tag: "unknown-variant", explanation: UNKNOWN_VARIANT },
        RuleInfo { tag: "effect-not-declared", explanation: EFFECT_NOT_DECLARED },
        RuleInfo { tag: "infinite-type", explanation: INFINITE_TYPE },
        RuleInfo { tag: "ambiguous-type", explanation: AMBIGUOUS_TYPE },
        RuleInfo { tag: "recursive-type-without-constructor", explanation: RECURSIVE_NO_CTOR },
        RuleInfo { tag: "refinement-violation", explanation: REFINEMENT_VIOLATION },
    ]
}

// Constants keyed off the tag so `all_rules` and
// `explanation_for_tag` produce identical strings without
// duplicating the prose.
const TYPE_MISMATCH: &str = "An expression's inferred type doesn't match what the surrounding context requires \
(return type, let-binding annotation, function argument, operator operand, etc.). Fix by changing \
the expression to produce the expected type, or by adjusting the declared/inferred expected type \
to match.";
const UNKNOWN_IDENT: &str = "A name referenced in scope is not declared. Either the binding is missing, \
the name is misspelled, or an `import` is missing. Check for typos first; then verify the relevant \
`let`, parameter, or top-level `fn` is in scope.";
const ARITY_MISMATCH: &str = "A call site supplies a different number of arguments than the function or \
constructor accepts. Either add the missing arguments or remove the extras.";
const NON_EXHAUSTIVE: &str = "A `match` expression doesn't cover every case of its scrutinee's type. \
Add the missing arms listed in the error, or add a `_` wildcard if catching the remainder is intended.";
const UNKNOWN_FIELD: &str = "A record field access or literal references a field name that isn't part of \
the record type. Verify spelling and that the type really has that field — check the type declaration.";
const DUPLICATE_FIELD: &str = "A record literal lists the same field name twice. Each field must appear \
exactly once. Remove the duplicate or rename one of them.";
const UNKNOWN_VARIANT: &str = "A constructor pattern or expression references a variant name that isn't \
part of the union type. Verify spelling and that the variant exists on this union.";
const EFFECT_NOT_DECLARED: &str = "A function body invokes an effect (io, fs_read, net, …) that the \
function's signature doesn't declare. Either add the effect to the function's `[effects]` annotation \
or remove the call that produces it.";
const INFINITE_TYPE: &str = "Inference would require a type to contain itself (e.g. `t = List<t>` with no \
constructor). Add a nominal type wrapper or restructure the data so the recursion is mediated by a \
named type.";
const AMBIGUOUS_TYPE: &str = "Inference couldn't pick a single concrete type for an expression. Add a type \
annotation to disambiguate.";
const RECURSIVE_NO_CTOR: &str = "A type alias references itself with no constructor in \
between, so no value of the type can ever be built. Make the recursive position carry a constructor \
(e.g. `Cons<T, List<T>> | Nil`).";
const REFINEMENT_VIOLATION: &str = "A literal argument provably violates a refinement-type predicate \
(#209). Adjust the argument to satisfy the predicate, or relax the predicate at the function \
signature.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_distinct_tag() {
        // Tags are stable identifiers; collisions would be a bug
        // and would silently merge two rule explanations.
        let tags: Vec<&str> = all_rules().iter().map(|r| r.tag).collect();
        let unique: std::collections::BTreeSet<&str> = tags.iter().copied().collect();
        assert_eq!(unique.len(), tags.len(), "rule tags must be unique: {tags:?}");
    }

    #[test]
    fn every_variant_has_a_nonempty_explanation() {
        for rule in all_rules() {
            assert!(!rule.explanation.is_empty(), "rule `{}` lacks an explanation", rule.tag);
            assert!(
                rule.explanation.len() > 40,
                "rule `{}` explanation is too short to be useful for LLM repair",
                rule.tag
            );
        }
    }

    #[test]
    fn type_error_methods_match_catalog() {
        // Pick a representative variant per rule and check the
        // method round-trips against `all_rules`.
        let cases: Vec<TypeError> = vec![
            TypeError::TypeMismatch {
                at_node: "n_0".into(),
                expected: "Int".into(),
                got: "Str".into(),
                context: vec![],
            },
            TypeError::UnknownIdentifier { at_node: "n_0".into(), name: "x".into() },
            TypeError::ArityMismatch { at_node: "n_0".into(), expected: 1, got: 2 },
            TypeError::NonExhaustiveMatch { at_node: "n_0".into(), missing: vec!["None".into()] },
            TypeError::UnknownField {
                at_node: "n_0".into(),
                record_type: "User".into(),
                field: "ag".into(),
            },
            TypeError::DuplicateField { at_node: "n_0".into(), field: "name".into() },
            TypeError::UnknownVariant { at_node: "n_0".into(), constructor: "Nada".into() },
            TypeError::EffectNotDeclared { at_node: "n_0".into(), effect: "io".into() },
            TypeError::InfiniteType { at_node: "n_0".into() },
            TypeError::AmbiguousType { at_node: "n_0".into() },
            TypeError::RecursiveTypeWithoutConstructor {
                at_node: "n_0".into(),
                name: "Bad".into(),
            },
            TypeError::RefinementViolation {
                at_node: "n_0".into(),
                fn_name: "f".into(),
                param_index: 0,
                binding: "x".into(),
                reason: "x > 0".into(),
            },
        ];
        let catalog: std::collections::BTreeMap<&str, &str> =
            all_rules().iter().map(|r| (r.tag, r.explanation)).collect();
        assert_eq!(cases.len(), catalog.len(), "every variant must be covered");
        for e in &cases {
            let tag = e.rule_tag();
            let expl = catalog.get(tag).unwrap_or_else(|| panic!("tag `{tag}` not in catalog"));
            assert_eq!(e.rule_explanation(), *expl, "tag/explanation mismatch on {tag}");
        }
    }
}
