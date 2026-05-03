//! Bare record patterns match nominal record types (closes #89).
//! Mirror of #79's literal coercion: `match v :: Bands { { idea: ..., execution: ... } => ... }`
//! should work even though the scrutinee is a `Ty::Con` aliasing a record.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;

fn check(src: &str) -> Result<(), Vec<lex_types::TypeError>> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).map(|_| ())
}

#[test]
fn bare_record_pattern_matches_nominal_record_alias() {
    let src = r#"
type Bands = { idea :: Str, execution :: Str }

fn verdict(b :: Bands) -> Str {
  match b {
    { idea: "high", execution: "high" } => "ship",
    _                                    => "iterate",
  }
}
"#;
    check(src).expect("structural pattern should match nominal record");
}

#[test]
fn bare_record_pattern_with_bindings_matches_nominal() {
    let src = r#"
type Pt = { x :: Int, y :: Int }

fn dist(p :: Pt) -> Int {
  match p {
    { x, y } => x + y,
  }
}
"#;
    check(src).expect("shorthand binders against nominal record");
}

#[test]
fn nested_bare_record_pattern_matches_nominal_inner() {
    let src = r#"
type Inner = { v :: Int }
type Outer = { inner :: Inner, name :: Str }

fn pull(o :: Outer) -> Int {
  match o {
    { inner: { v }, name: _ } => v,
  }
}
"#;
    check(src).expect("nested bare record pattern should match nominal inner");
}

#[test]
fn bare_record_pattern_still_works_on_anonymous_record() {
    // Regression: don't break the existing structural-on-structural case.
    let src = r#"
fn pick(p :: { x :: Int, y :: Int }) -> Int {
  match p {
    { x, y: _ } => x,
  }
}
"#;
    check(src).expect("structural pattern on structural type");
}

#[test]
fn bare_record_pattern_against_non_record_still_errors() {
    let src = r#"
fn nope(n :: Int) -> Int {
  match n {
    { x } => x,
    _     => n,
  }
}
"#;
    check(src).expect_err("record pattern against Int should reject");
}

#[test]
fn bare_record_pattern_unknown_field_against_nominal_still_errors() {
    let src = r#"
type Pt = { x :: Int, y :: Int }

fn nope(p :: Pt) -> Int {
  match p {
    { z } => z,
  }
}
"#;
    check(src).expect_err("unknown field in record pattern should reject");
}
