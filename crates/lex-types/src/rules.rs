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

/// Static (rule_tag → suggested_transform) table for #306 slice 3.
///
/// When `Store::apply_operation_checked` rejects an op for a
/// `TypeError`, the gate consults this table and pre-populates the
/// `RepairHint` attestation's `suggested_transform` payload so the
/// LLM repair flow (or a human reading `lex repair <op>`) has a
/// concrete starting point. `None` means no static suggestion
/// exists for this rule; the LLM-driven `lex repair --apply` path
/// still works.
///
/// The returned shape is a JSON object with:
/// - `kind_hint`: name of the typed transform most likely to fix it
///   (`"ReplaceMatchArm"`, `"RenameLocal"`, `"InlineLet"`,
///   `"ChangeEffectSig"`, `"ModifyBody"`).
/// - `rule_tag`: echo of the rule that fired (for downstream
///   correlation).
/// - `summary`: one-sentence direction.
/// - `details`: longer prose suitable for an LLM repair prompt.
pub fn suggested_transform_for(rule_tag: &str) -> Option<serde_json::Value> {
    let (kind_hint, summary, details) = match rule_tag {
        "type-mismatch" => (
            "ReplaceMatchArm",
            "Replace the offending match arm (or expression) so its body produces the expected type.",
            "When a function body's inferred type doesn't match its signature, the easiest \
typed-transform fix is `ReplaceMatchArm` — rebuild whichever arm produces the wrong type so it \
returns the expected one. For non-match expressions, the LLM-driven `lex repair --apply` flow \
can rewrite the body via `ModifyBody`.",
        ),
        "unknown-identifier" => (
            "RenameLocal",
            "If the name is a typo, rename a similarly-spelled in-scope binding to match.",
            "An `unknown-identifier` error is most often a typo. Search the function's lexical \
scope for a binding whose name is a single edit away and apply `RenameLocal` to switch references. \
If no nearby name exists, the missing binding probably needs a `let` or an `import` — fall back to \
LLM-driven repair.",
        ),
        "non-exhaustive-match" => (
            "ReplaceMatchArm",
            "Add the missing match arms (or a `_` wildcard) covering the unhandled variants.",
            "Use `ReplaceMatchArm` to append arms for the variants listed in the error's \
`missing` field. If catching the remainder is intended, a single `_` wildcard arm suffices; \
otherwise add one explicit arm per missing variant so the audit trail records the new semantics.",
        ),
        "effect-not-declared" => (
            "ChangeEffectSig",
            "Add the inferred effect to the function's `[effects]` declaration.",
            "The function body invokes an effect that the signature doesn't declare. Either add \
the effect to the signature via `ChangeEffectSig` (preferred — the effect is genuinely needed) or \
remove the call that produces it via `ModifyBody` (preferred when the effect was unintentional).",
        ),
        "arity-mismatch" => (
            "ModifyBody",
            "Match the call site's argument count to the function's declared arity.",
            "The number of arguments at the call site doesn't match the declared signature. \
Add the missing arguments or remove the extras. No typed transform directly applies — \
use the LLM-driven `lex repair --apply` flow with `ModifyBody` to rewrite the call site.",
        ),
        "unknown-field" => (
            "ModifyBody",
            "Verify the field spelling and the record type's declaration; rewrite the access.",
            "The field name isn't part of the record type. Either correct the spelling or add \
the missing field to the type declaration. Use the LLM-driven `lex repair --apply` flow with \
`ModifyBody` to rewrite the field access once the correct name is known.",
        ),
        "ambiguous-type" => (
            "ModifyBody",
            "Add a type annotation at the ambiguous expression to disambiguate inference.",
            "Inference couldn't pick a single concrete type. Add an explicit type annotation on \
the offending `let` binding, function parameter, or function return type via `ModifyBody`. The \
LLM-driven `lex repair --apply` flow can synthesize the annotation from the surrounding context.",
        ),
        _ => return None,
    };
    Some(serde_json::json!({
        "kind_hint": kind_hint,
        "rule_tag": rule_tag,
        "summary": summary,
        "details": details,
    }))
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

    #[test]
    fn suggested_transform_covers_at_least_five_rules() {
        // #306 slice 3 AC: ≥5 rule_tags have a non-None
        // suggested_transform. Catches accidental removal.
        let mut covered = 0;
        for rule in all_rules() {
            if suggested_transform_for(rule.tag).is_some() {
                covered += 1;
            }
        }
        assert!(
            covered >= 5,
            "suggested_transform must cover ≥5 rule_tags; got {covered}"
        );
    }

    #[test]
    fn suggested_transform_shape_is_consistent() {
        // Every non-None suggestion must carry the four fields
        // documented in `suggested_transform_for`.
        for rule in all_rules() {
            let Some(s) = suggested_transform_for(rule.tag) else { continue };
            for field in ["kind_hint", "rule_tag", "summary", "details"] {
                assert!(
                    s.get(field).and_then(|v| v.as_str()).is_some_and(|v| !v.is_empty()),
                    "rule `{}` suggestion missing/empty `{field}`: {s}",
                    rule.tag
                );
            }
            assert_eq!(
                s.get("rule_tag").and_then(|v| v.as_str()),
                Some(rule.tag),
                "suggestion's rule_tag must echo the input tag"
            );
        }
    }

    #[test]
    fn unknown_rule_tag_returns_none() {
        assert!(suggested_transform_for("does-not-exist").is_none());
    }
}
