//! Acceptance tests for #228: dead-branch elimination.
//!
//! `if true { ... } else { ... }` (and its `match`-over-Bool-literal
//! desugared form) folds to the live arm. Critically, this runs
//! *before* type-checking, so the inferred effect set drops any
//! `[net]` / `[fs_*]` etc. that lived only in the dead branch.

use lex_ast::canonical::{CExpr, CLit, Stage};
use lex_ast::canonicalize_program;
use lex_syntax::parse_source;

fn canon(src: &str) -> Vec<Stage> {
    let p = parse_source(src).expect("parse");
    canonicalize_program(&p)
}

fn fn_body<'a>(stages: &'a [Stage], name: &str) -> &'a CExpr {
    for s in stages {
        if let Stage::FnDecl(fd) = s {
            if fd.name == name { return &fd.body; }
        }
    }
    panic!("no fn named {name}");
}

#[test]
fn if_true_folds_to_then_branch() {
    let stages = canon("fn pick() -> Int { if true { 1 } else { 2 } }");
    // Body should be Block { result: 1 } or just Literal { 1 }
    // depending on how blocks unfold. Either way, no Match remains.
    let body = fn_body(&stages, "pick");
    assert!(!contains_match(body),
        "expected no Match in folded body; got: {body:?}");
}

#[test]
fn if_false_folds_to_else_branch() {
    let stages = canon("fn pick() -> Int { if false { 1 } else { 2 } }");
    let body = fn_body(&stages, "pick");
    assert!(!contains_match(body),
        "expected no Match in folded body; got: {body:?}");
    // Result should ultimately be the literal 2.
    assert!(yields_int(body, 2),
        "expected the body to yield Int(2); got: {body:?}");
}

#[test]
fn nested_constant_branches_collapse_in_one_pass() {
    // if true { if false { 1 } else { 2 } } else { 3 }
    //   →  match (inner): 2
    //   →  match (outer): 2
    let stages = canon(r#"
fn pick() -> Int {
  if true {
    if false { 1 } else { 2 }
  } else {
    3
  }
}
"#);
    let body = fn_body(&stages, "pick");
    assert!(!contains_match(body),
        "expected all Match nodes collapsed; got: {body:?}");
    assert!(yields_int(body, 2),
        "expected body to yield Int(2); got: {body:?}");
}

#[test]
fn non_literal_predicate_is_not_folded() {
    // The predicate is a parameter — nothing to fold.
    let stages = canon(r#"
fn pick(b :: Bool) -> Int {
  if b { 1 } else { 2 }
}
"#);
    let body = fn_body(&stages, "pick");
    assert!(contains_match(body),
        "non-literal predicate should leave the Match intact; got: {body:?}");
}

#[test]
fn match_over_int_literal_is_folded() {
    // The pass also handles non-bool literal scrutinees, which falls
    // out of the same rule — useful for state-machine patterns where
    // an agent emits `match status { 1 => ..., 2 => ... }` with a
    // constant `status`.
    let stages = canon(r#"
fn pick() -> Str {
  match 2 {
    1 => "one",
    2 => "two",
    _ => "other",
  }
}
"#);
    let body = fn_body(&stages, "pick");
    assert!(!contains_match(body),
        "expected the Int-literal Match to fold; got: {body:?}");
}

#[test]
fn wildcard_arm_is_taken_when_no_literal_matches() {
    let stages = canon(r#"
fn pick() -> Str {
  match 99 {
    1 => "one",
    2 => "two",
    _ => "fallback",
  }
}
"#);
    let body = fn_body(&stages, "pick");
    assert!(!contains_match(body));
    // Best-effort: the body should ultimately produce "fallback".
    assert!(yields_str(body, "fallback"),
        "expected body to yield \"fallback\"; got: {body:?}");
}

// (Effect-set soundness — the headline acceptance criterion — is
// pinned by `lex-runtime/tests/dead_branch_effects.rs` since this
// crate doesn't have lex-types as a dependency.)

// ---- helpers ----

fn contains_match(e: &CExpr) -> bool {
    use CExpr::*;
    match e {
        Match { .. } => true,
        Call { callee, args } =>
            contains_match(callee) || args.iter().any(contains_match),
        Let { value, body, .. } => contains_match(value) || contains_match(body),
        Block { statements, result } =>
            statements.iter().any(contains_match) || contains_match(result),
        Constructor { args, .. } => args.iter().any(contains_match),
        RecordLit { fields } => fields.iter().any(|f| contains_match(&f.value)),
        TupleLit { items } | ListLit { items } => items.iter().any(contains_match),
        FieldAccess { value, .. } => contains_match(value),
        Lambda { body, .. } => contains_match(body),
        BinOp { lhs, rhs, .. } => contains_match(lhs) || contains_match(rhs),
        UnaryOp { expr, .. } => contains_match(expr),
        Return { value } => contains_match(value),
        Literal { .. } | Var { .. } => false,
    }
}

fn yields_int(e: &CExpr, n: i64) -> bool {
    match e {
        CExpr::Literal { value: CLit::Int { value } } => *value == n,
        CExpr::Block { result, .. } => yields_int(result, n),
        _ => false,
    }
}

fn yields_str(e: &CExpr, s: &str) -> bool {
    match e {
        CExpr::Literal { value: CLit::Str { value } } => value == s,
        CExpr::Block { result, .. } => yields_str(result, s),
        _ => false,
    }
}
