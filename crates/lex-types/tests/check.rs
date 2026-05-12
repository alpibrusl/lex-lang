//! M3 acceptance: every example in §3.13 type-checks; structured errors fire.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{check_program, TypeError};

fn check(src: &str) -> Result<(), Vec<TypeError>> {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    check_program(&stages).map(|_| ())
}

#[test]
fn example_a_factorial_checks() {
    let src = include_str!("../../../examples/a_factorial.lex");
    check(src).unwrap_or_else(|errs| panic!("type errors: {errs:#?}"));
}

#[test]
fn example_b_parse_int_checks() {
    let src = include_str!("../../../examples/b_parse_int.lex");
    check(src).unwrap_or_else(|errs| panic!("type errors: {errs:#?}"));
}

#[test]
fn example_c_echo_checks() {
    let src = include_str!("../../../examples/c_echo.lex");
    check(src).unwrap_or_else(|errs| panic!("type errors: {errs:#?}"));
}

#[test]
fn example_d_shape_checks() {
    let src = include_str!("../../../examples/d_shape.lex");
    check(src).unwrap_or_else(|errs| panic!("type errors: {errs:#?}"));
}

#[test]
fn detects_type_mismatch() {
    let src = "fn bad(x :: Int) -> Str { x }\n";
    let errs = check(src).unwrap_err();
    assert!(matches!(errs[0], TypeError::TypeMismatch { .. }));
}

#[test]
fn detects_unknown_identifier() {
    let src = "fn bad() -> Int { y }\n";
    let errs = check(src).unwrap_err();
    assert!(matches!(errs[0], TypeError::UnknownIdentifier { .. }));
}

#[test]
fn detects_arity_mismatch() {
    let src = "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn bad() -> Int { add(1) }\n";
    let errs = check(src).unwrap_err();
    assert!(matches!(errs[0], TypeError::ArityMismatch { .. }));
}

#[test]
fn detects_undeclared_effect() {
    let src = r#"
import "std.io" as io
fn bad() -> Str {
  match io.read("path") {
    Ok(s) => s,
    Err(e) => e,
  }
}
"#;
    let errs = check(src).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, TypeError::EffectNotDeclared { .. })),
        "expected effect_not_declared, got {errs:#?}");
}

#[test]
fn detects_unknown_field() {
    let src = "fn bad() -> Int { let r := { x: 1 }\n r.y }\n";
    let errs = check(src).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, TypeError::UnknownField { .. })),
        "got {errs:#?}");
}

#[test]
fn detects_unknown_variant() {
    // Use a non-literal scrutinee so the dead-branch elimination pass
    // (which runs before type-check) doesn't fold away the Bogus arm
    // before the type-checker sees it.
    let src = "fn bad(n :: Int) -> Int { match n { Bogus(x) => x, _ => 0 } }\n";
    let errs = check(src).unwrap_err();
    assert!(errs.iter().any(|e| matches!(e, TypeError::UnknownVariant { .. })),
        "got {errs:#?}");
}

#[test]
fn every_error_has_node_id() {
    let src = "fn bad(x :: Int) -> Str { x }\n";
    let errs = check(src).unwrap_err();
    for e in &errs { assert!(!e.node().is_empty(), "missing node id: {e:?}"); }
}

// --- #369: signature-level examples ---

#[test]
fn examples_with_matching_types_check() {
    let src = "fn id(x :: Int) -> Int\nexamples { id(7) => 7, id(0) => 0 }\n{ x }\n";
    check(src).unwrap_or_else(|errs| panic!("type errors: {errs:#?}"));
}

#[test]
fn example_arg_type_mismatch_is_caught() {
    let src = "fn id(x :: Int) -> Int\nexamples { id(\"oops\") => 7 }\n{ x }\n";
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::TypeMismatch { .. })),
        "expected TypeMismatch, got {errs:#?}"
    );
}

#[test]
fn example_expected_type_mismatch_is_caught() {
    let src = "fn id(x :: Int) -> Int\nexamples { id(7) => \"seven\" }\n{ x }\n";
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::TypeMismatch { .. })),
        "expected TypeMismatch, got {errs:#?}"
    );
}

#[test]
fn example_arity_mismatch_is_caught() {
    let src = "fn add(x :: Int, y :: Int) -> Int\nexamples { add(1) => 1 }\n{ x + y }\n";
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::ExampleArityMismatch { .. })),
        "expected ExampleArityMismatch, got {errs:#?}"
    );
}

#[test]
fn examples_on_effectful_fn_are_rejected() {
    let src = r#"
import "std.io" as io
fn echoes(s :: Str) -> [io] Str
  examples { echoes("hi") => "hi" }
{ s }
"#;
    let errs = check(src).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(e, TypeError::ExamplesOnEffectfulFn { .. })),
        "expected ExamplesOnEffectfulFn, got {errs:#?}"
    );
}

#[test]
fn example_rule_tags_round_trip() {
    // Both new rule tags must be reachable via `TypeError::rule_tag()`.
    let arity = TypeError::ExampleArityMismatch {
        at_node: "n_0".into(),
        fn_name: "f".into(),
        case_index: 0,
        expected: 2,
        got: 1,
    };
    let effectful = TypeError::ExamplesOnEffectfulFn {
        at_node: "n_0".into(),
        fn_name: "f".into(),
    };
    assert_eq!(arity.rule_tag(), "example-arity-mismatch");
    assert_eq!(effectful.rule_tag(), "examples-on-effectful-fn");
}
