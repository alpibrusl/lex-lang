//! Integration tests for `std.test`. The module is itself an
//! assertion library, so the tests below verify that the four
//! helpers return the expected `Result[Unit, Str]` shape on both
//! the pass and fail paths.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

const SRC: &str = r#"
import "std.test" as test
import "std.list" as list

fn eq_pass(a :: Int, b :: Int) -> Result[Unit, Str] { test.assert_eq(a, b) }
fn ne_pass(a :: Int, b :: Int) -> Result[Unit, Str] { test.assert_ne(a, b) }
fn t_pass(b :: Bool) -> Result[Unit, Str] { test.assert_true(b) }
fn f_pass(b :: Bool) -> Result[Unit, Str] { test.assert_false(b) }

# A tiny suite runner — exactly the shape the user's tests/
# directory would carry. Each test is `() -> Result[Unit, Str]`
# but Lex doesn't have closures-as-values in records, so the
# suite here is a list of pre-computed verdicts.
fn run_suite(verdicts :: List[Result[Unit, Str]]) -> Int {
  list.fold(verdicts, 0, fn (acc :: Int, v :: Result[Unit, Str]) -> Int {
    match v {
      Ok(_)  => acc,
      Err(_) => acc + 1,
    }
  })
}
"#;

fn assert_ok(v: &Value) {
    match v {
        Value::Variant { name, .. } if name == "Ok" => {}
        other => panic!("expected Ok, got {other:?}"),
    }
}

fn assert_err_contains(v: &Value, needle: &str) {
    match v {
        Value::Variant { name, args } if name == "Err" => match &args[0] {
            Value::Str(s) => assert!(s.contains(needle), "Err {s:?} missing {needle:?}"),
            other => panic!("expected Err(Str), got {other:?}"),
        },
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn assert_eq_passes_and_fails() {
    assert_ok(&run(SRC, "eq_pass", vec![Value::Int(1), Value::Int(1)]));
    assert_err_contains(
        &run(SRC, "eq_pass", vec![Value::Int(1), Value::Int(2)]),
        "assert_eq",
    );
}

#[test]
fn assert_ne_passes_and_fails() {
    assert_ok(&run(SRC, "ne_pass", vec![Value::Int(1), Value::Int(2)]));
    assert_err_contains(
        &run(SRC, "ne_pass", vec![Value::Int(7), Value::Int(7)]),
        "assert_ne",
    );
}

#[test]
fn assert_true_passes_and_fails() {
    assert_ok(&run(SRC, "t_pass", vec![Value::Bool(true)]));
    assert_err_contains(&run(SRC, "t_pass", vec![Value::Bool(false)]), "assert_true");
}

#[test]
fn assert_false_passes_and_fails() {
    assert_ok(&run(SRC, "f_pass", vec![Value::Bool(false)]));
    assert_err_contains(&run(SRC, "f_pass", vec![Value::Bool(true)]), "assert_false");
}

#[test]
fn suite_runner_counts_failures() {
    // Composing three Results into a list and reducing with foldl
    // is exactly how a user-side test suite would tally verdicts.
    let suite = Value::List(vec![
        ok_unit(),                              // pass
        err_msg("first failure"),               // fail
        ok_unit(),                              // pass
        err_msg("second failure"),              // fail
    ]);
    let v = run(SRC, "run_suite", vec![suite]);
    assert_eq!(v, Value::Int(2));
}

fn ok_unit() -> Value {
    Value::Variant { name: "Ok".into(), args: vec![Value::Unit] }
}
fn err_msg(s: &str) -> Value {
    Value::Variant { name: "Err".into(), args: vec![Value::Str(s.into())] }
}
