//! Acceptance tests for #209 slice 1: parser + AST plumbing for
//! refinement types.
//!
//! Slice scope: signatures with `Type{x | predicate}` parse, survive
//! canonicalization, and pass through the type checker by unifying
//! structurally as their base type. Static discharge (proving
//! `withdraw(5)` against `amount > 0` at the call site) and the
//! residual runtime check at call boundaries land in slices 2 and 3.
//!
//! What this slice deliberately does *not* test:
//!   - Static discharge of the predicate at call sites.
//!   - Runtime residual checks recorded in lex-trace.
//!   - lex-vcs `ChangeEffectSig` carrying refinement diffs (the
//!     canonical AST already includes the predicate, so OpId hashing
//!     picks up changes — but the surfaced diagnostic shape is a
//!     slice-3 concern).

use lex_ast::canonicalize_program;
use lex_ast::TypeExpr;
use lex_syntax::parse_source;

#[test]
fn signature_with_refined_int_param_parses() {
    let src = r#"
fn withdraw(amount :: Int{x | x > 0}) -> Int { amount }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let fd = match &stages[0] {
        lex_ast::Stage::FnDecl(fd) => fd,
        other => panic!("expected FnDecl, got {other:?}"),
    };
    let param_ty = &fd.params[0].ty;
    match param_ty {
        TypeExpr::Refined { base, binding, .. } => {
            assert_eq!(binding, "x");
            assert!(matches!(base.as_ref(), TypeExpr::Named { name, .. } if name == "Int"));
        }
        other => panic!("expected Refined param type, got {other:?}"),
    }
}

#[test]
fn refinement_with_compound_predicate_parses() {
    // The headline example from #209: `Int{x | x > 0 and x <= balance}`.
    let src = r#"
fn pay(amount :: Int{x | x > 0 and x <= 100}) -> Int { amount }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let fd = match &stages[0] {
        lex_ast::Stage::FnDecl(fd) => fd,
        _ => unreachable!(),
    };
    assert!(matches!(&fd.params[0].ty, TypeExpr::Refined { .. }));
}

#[test]
fn refined_param_unifies_as_its_base_type_for_now() {
    // Slice 1: refined types are structurally equal to their base.
    // A function declaring `Int{x | x > 0}` accepts plain Int callers
    // and type-checks. Slice 2 will tighten this so static-known
    // violations are rejected.
    let src = r#"
fn withdraw(amount :: Int{x | x > 0}) -> Int { amount }
fn caller() -> Int { withdraw(-5) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    // Type-check should pass: refined Int unifies as Int.
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("expected type-check pass (slice 1 unifies refined as \
                base); got: {errs:#?}");
    }
}

#[test]
fn refinement_on_list_type_parses() {
    // From the issue: `List[T]{xs | length(xs) > 0}` — refinement
    // on a parametric type. Confirms the postfix lookahead works
    // after a generic-arg-bearing base.
    let src = r#"
fn first(xs :: List[Int]{ys | true}) -> Int { 0 }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let fd = match &stages[0] {
        lex_ast::Stage::FnDecl(fd) => fd,
        _ => unreachable!(),
    };
    match &fd.params[0].ty {
        TypeExpr::Refined { base, binding, .. } => {
            assert_eq!(binding, "ys");
            assert!(matches!(base.as_ref(), TypeExpr::Named { name, args }
                if name == "List" && args.len() == 1));
        }
        other => panic!("expected Refined, got {other:?}"),
    }
}

#[test]
fn function_body_braces_are_not_mistaken_for_refinement() {
    // Disambiguation guard: `-> Int { body }` is a function with body,
    // not `Int{body}` refinement. Refinements need `{ Ident |` ahead;
    // bodies don't have that lookahead shape.
    let src = r#"
fn answer() -> Int { 42 }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let fd = match &stages[0] {
        lex_ast::Stage::FnDecl(fd) => fd,
        _ => unreachable!(),
    };
    assert!(matches!(&fd.return_type, TypeExpr::Named { name, .. } if name == "Int"));
}

#[test]
fn refinement_on_return_type_parses() {
    let src = r#"
fn pos() -> Int{x | x > 0} { 7 }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let fd = match &stages[0] {
        lex_ast::Stage::FnDecl(fd) => fd,
        _ => unreachable!(),
    };
    assert!(matches!(&fd.return_type, TypeExpr::Refined { .. }));
}

#[test]
fn predicate_is_carried_through_canonicalization() {
    // The refinement predicate must reach the canonical AST so
    // lex-vcs's content-addressing picks up changes to it. Two
    // signatures differing only in the predicate produce structurally
    // distinct canonical TypeExprs.
    let src_a = "fn f(x :: Int{n | n > 0}) -> Int { x }";
    let src_b = "fn f(x :: Int{n | n > 1}) -> Int { x }";
    let stages_a = canonicalize_program(&parse_source(src_a).expect("parse"));
    let stages_b = canonicalize_program(&parse_source(src_b).expect("parse"));
    let ty_a = match &stages_a[0] {
        lex_ast::Stage::FnDecl(fd) => &fd.params[0].ty,
        _ => unreachable!(),
    };
    let ty_b = match &stages_b[0] {
        lex_ast::Stage::FnDecl(fd) => &fd.params[0].ty,
        _ => unreachable!(),
    };
    assert_ne!(ty_a, ty_b,
        "refinement predicates must reach the canonical AST so changes \
         to them perturb the type's identity");
}
