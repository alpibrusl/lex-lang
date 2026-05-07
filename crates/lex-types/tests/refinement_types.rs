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
    // Refined types unify structurally as their base, so a non-literal
    // call (where slice-2 discharge can't decide) type-checks. Use a
    // local var so the call defers to slice 3's runtime check.
    let src = r#"
fn withdraw(amount :: Int{x | x > 0}) -> Int { amount }
fn caller(input :: Int) -> Int { withdraw(input) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    // Type-check should pass: refined Int unifies as Int and the
    // non-literal arg defers to runtime (slice 3 territory).
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("expected type-check pass (non-literal arg defers to \
                runtime check); got: {errs:#?}");
    }
}

// ---- #209 slice 2: static discharge of literal arguments ---------

#[test]
fn literal_arg_satisfying_predicate_proves_statically() {
    // The headline acceptance criterion: `withdraw(5)` against
    // `amount > 0` proves at compile time, no runtime cost.
    let src = r#"
fn withdraw(amount :: Int{x | x > 0}) -> Int { amount }
fn caller() -> Int { withdraw(5) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("expected static discharge to prove `5 > 0`; \
                got: {errs:#?}");
    }
}

#[test]
fn literal_arg_violating_predicate_is_refuted_statically() {
    // Pre-slice-2 this type-checked silently; now `withdraw(-5)`
    // is rejected at compile time because the type checker can
    // evaluate the predicate `x > 0` with `x = -5` and see it's
    // false. The headline correctness win.
    let src = r#"
fn withdraw(amount :: Int{x | x > 0}) -> Int { amount }
fn caller() -> Int { withdraw(-5) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let errs = match lex_types::check_program(&stages) {
        Ok(_) => panic!("expected type-check to fail"),
        Err(e) => e,
    };
    let viol = errs.iter().find(|e| matches!(e,
        lex_types::TypeError::RefinementViolation { .. }));
    assert!(viol.is_some(),
        "expected RefinementViolation; got: {errs:#?}");
}

#[test]
fn compound_predicate_proves_when_all_clauses_hold() {
    let src = r#"
fn pay(amount :: Int{x | x > 0 and x <= 100}) -> Int { amount }
fn caller() -> Int { pay(50) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("50 satisfies (>0 and <=100)");
}

#[test]
fn compound_predicate_refutes_on_upper_bound() {
    let src = r#"
fn pay(amount :: Int{x | x > 0 and x <= 100}) -> Int { amount }
fn caller() -> Int { pay(150) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let errs = match lex_types::check_program(&stages) {
        Ok(_) => panic!("expected type-check to fail"),
        Err(e) => e,
    };
    assert!(errs.iter().any(|e| matches!(e,
        lex_types::TypeError::RefinementViolation { .. })),
        "expected RefinementViolation for 150 > 100; got: {errs:#?}");
}

#[test]
fn predicate_with_external_var_defers_to_runtime() {
    // The predicate references `balance`, which isn't the binding.
    // Slice 2's discharge engine doesn't try to resolve external
    // identifiers; it defers to slice 3's runtime check. The literal
    // arg by itself doesn't statically violate the part the engine
    // *can* see, so the call type-checks.
    //
    // This intentionally documents a slice-2 limitation: agents that
    // want richer discharge today should rewrite predicates so all
    // free variables are the binding (e.g. inline `balance` as a
    // numeric constant). Slice 3 will plumb call-site context.
    let src = r#"
fn withdraw(amount :: Int{x | x > 0 and x <= balance}) -> Int { amount }
fn caller() -> Int { withdraw(50) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect(
        "external `balance` should defer; the bound predicate `x > 0` \
         alone is satisfied by 50, so no static refutation");
}

#[test]
fn refutation_error_names_the_binding_and_reason() {
    let src = r#"
fn pos(amount :: Int{x | x > 0}) -> Int { amount }
fn caller() -> Int { pos(-3) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let errs = match lex_types::check_program(&stages) {
        Ok(_) => panic!("expected type-check to fail"),
        Err(e) => e,
    };
    let v = errs.iter().find_map(|e| match e {
        lex_types::TypeError::RefinementViolation {
            fn_name, binding, reason, ..
        } => Some((fn_name.clone(), binding.clone(), reason.clone())),
        _ => None,
    }).expect("expected RefinementViolation");
    assert_eq!(v.0, "pos");
    assert_eq!(v.1, "x");
    assert!(v.2.contains("-3"),
        "reason should name the failing arg value; got: {}", v.2);
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
