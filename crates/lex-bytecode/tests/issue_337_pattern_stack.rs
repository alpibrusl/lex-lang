//! Regression for #337 — constructor-pattern fail path used to leak
//! the scrutinee onto the VM stack. A `match` against an ADT
//! scrutinee whose pattern took the `_` arm would panic if any
//! enclosing context expected a clean stack (e.g. `false or match
//! …`, or any wildcard arm whose body referenced an unrelated
//! value).
//!
//! Fix: the `PConstructor` branch in `compile_pattern_test` now
//! routes failure through an explicit `Pop` + `Jump` so the dup'd
//! scrutinee is dropped before jumping to the next arm.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Value, Vm};
use lex_syntax::parse_source;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let prog = compile_program(&stages);
    let mut vm = Vm::new(&prog);
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

const STR_CHECK: &str = r#"
type StrCheck = StrEmail | StrMinLen(Int) | StrMaxLen(Int)

fn test(chk :: StrCheck) -> Bool {
  false or match chk { StrEmail => true, _ => false }
}

fn t_email() -> Bool      { test(StrEmail) }
fn t_min_len() -> Bool    { test(StrMinLen(1)) }
fn t_max_len() -> Bool    { test(StrMaxLen(99)) }
"#;

#[test]
fn issue_337_match_in_or_with_zero_arg_variant_matches() {
    assert_eq!(run(STR_CHECK, "t_email", vec![]), Value::Bool(true));
}

#[test]
fn issue_337_match_in_or_with_payload_variant_falls_through() {
    // The original panic. Previously: "expected Bool, got
    // Variant { name: \"StrMinLen\", args: [Int(1)] }"
    // because the failing TestVariant left the scrutinee on the
    // stack and subsequent ops popped it instead of the wildcard's
    // result.
    assert_eq!(run(STR_CHECK, "t_min_len", vec![]), Value::Bool(false));
    assert_eq!(run(STR_CHECK, "t_max_len", vec![]), Value::Bool(false));
}

#[test]
fn issue_337_nested_constructor_pattern_round_trips() {
    // Nested PConstructor: both outer and inner pattern tests use
    // the new Pop-then-Jump shape on failure. Three branches:
    // exact match, outer matches inner doesn't, neither matches.
    let src = r#"
fn cls(v :: Result[Option[Int], Str]) -> Int {
  match v {
    Ok(Some(n)) => n,
    Ok(None)    => -1,
    Err(_)      => -2,
  }
}
fn a() -> Int { cls(Ok(Some(42))) }
fn b() -> Int { cls(Ok(None)) }
fn c() -> Int { cls(Err("oops")) }
"#;
    assert_eq!(run(src, "a", vec![]), Value::Int(42));
    assert_eq!(run(src, "b", vec![]), Value::Int(-1));
    assert_eq!(run(src, "c", vec![]), Value::Int(-2));
}

#[test]
fn issue_337_wildcard_after_failed_constructor_returns_correct_value() {
    // Direct test that the wildcard arm's body value isn't poisoned
    // by a leaked scrutinee. Returning a fresh Int from the `_` arm
    // would previously surface the variant value via the leak.
    let src = r#"
type Tag = TagA | TagB | TagC

fn pick(t :: Tag) -> Int {
  match t {
    TagA => 1,
    _    => 99,
  }
}
fn a() -> Int { pick(TagA) }
fn b() -> Int { pick(TagB) }
"#;
    assert_eq!(run(src, "a", vec![]), Value::Int(1));
    assert_eq!(run(src, "b", vec![]), Value::Int(99));
}
