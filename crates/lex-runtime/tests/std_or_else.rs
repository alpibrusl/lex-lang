//! Integration tests for the recovery combinators `result.or_else`
//! and `option.or_else`. Closes the remaining gap on #212.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.result" as res
import "std.option" as opt

# Recover from an Err by mapping the error to a fallback Ok.
fn recover_to_zero(r :: Result[Int, Str]) -> Result[Int, Str] {
  res.or_else(r,
    fn (_e :: Str) -> Result[Int, Str] { Ok(0) })
}

# Closure must NOT run when the input is already Ok.
fn ok_short_circuits() -> Result[Int, Str] {
  res.or_else(Ok(7),
    fn (_e :: Str) -> Result[Int, Str] { Ok(99) })
}

# or_else can swap the error type (E1 -> E2).
fn rewrite_error(r :: Result[Int, Str]) -> Result[Int, Int] {
  res.or_else(r,
    fn (_e :: Str) -> Result[Int, Int] { Err(0 - 1) })
}

# option.or_else: replace None with the closure's Option.
fn fallback_some() -> Option[Int] {
  opt.or_else(None,
    fn () -> Option[Int] { Some(42) })
}

# option.or_else: Some short-circuits the closure.
fn some_short_circuits() -> Option[Int] {
  opt.or_else(Some(7),
    fn () -> Option[Int] { Some(99) })
}

# option.or_else: closure may itself return None.
fn fallback_still_none() -> Option[Int] {
  opt.or_else(None,
    fn () -> Option[Int] { None })
}
"#;

fn run(fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn variant<'v>(v: &'v Value) -> (&'v str, &'v [Value]) {
    match v {
        Value::Variant { name, args } => (name.as_str(), args.as_slice()),
        other => panic!("expected Variant, got {other:?}"),
    }
}

fn ok(x: i64) -> Value {
    Value::Variant { name: "Ok".into(), args: vec![Value::Int(x)] }
}

fn err(s: &str) -> Value {
    Value::Variant { name: "Err".into(), args: vec![Value::Str(s.into())] }
}

#[test]
fn result_or_else_recovers_err() {
    let v = run("recover_to_zero", vec![err("boom")]);
    let (name, args) = variant(&v);
    assert_eq!(name, "Ok");
    assert_eq!(args.first(), Some(&Value::Int(0)));
}

#[test]
fn result_or_else_passes_through_ok() {
    let v = run("recover_to_zero", vec![ok(99)]);
    let (name, args) = variant(&v);
    assert_eq!(name, "Ok");
    assert_eq!(args.first(), Some(&Value::Int(99)));
}

#[test]
fn result_or_else_does_not_run_closure_on_ok() {
    // The closure would replace the value with 99 if it ran;
    // confirming we still see the original 7 pins the short-circuit.
    let v = run("ok_short_circuits", vec![]);
    let (name, args) = variant(&v);
    assert_eq!(name, "Ok");
    assert_eq!(args.first(), Some(&Value::Int(7)));
}

#[test]
fn result_or_else_can_swap_error_type() {
    let v = run("rewrite_error", vec![err("boom")]);
    let (name, args) = variant(&v);
    assert_eq!(name, "Err");
    assert_eq!(args.first(), Some(&Value::Int(-1)));
}

#[test]
fn option_or_else_replaces_none() {
    let v = run("fallback_some", vec![]);
    let (name, args) = variant(&v);
    assert_eq!(name, "Some");
    assert_eq!(args.first(), Some(&Value::Int(42)));
}

#[test]
fn option_or_else_passes_through_some() {
    let v = run("some_short_circuits", vec![]);
    let (name, args) = variant(&v);
    assert_eq!(name, "Some");
    assert_eq!(args.first(), Some(&Value::Int(7)));
}

#[test]
fn option_or_else_closure_may_return_none() {
    let v = run("fallback_still_none", vec![]);
    let (name, _) = variant(&v);
    assert_eq!(name, "None");
}
