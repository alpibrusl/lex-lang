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
    let src = "fn bad() -> Int { match 1 { Bogus(x) => x, _ => 0 } }\n";
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
