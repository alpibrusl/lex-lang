//! std.flow extensions: `parallel_list` (variadic counterpart to
//! `parallel`). Spec §11.2 — sequential under the hood; threading is
//! reserved for a future scheduler.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

#[test]
fn parallel_list_runs_three_actions_in_input_order() {
    let src = r#"
import "std.flow" as flow
fn run3() -> List[Int] {
  flow.parallel_list([
    fn () -> Int { 7 },
    fn () -> Int { 11 },
    fn () -> Int { 13 }
  ])
}
"#;
    let r = run(src, "run3", vec![]);
    assert_eq!(
        r,
        Value::List(vec![Value::Int(7), Value::Int(11), Value::Int(13)])
    );
}

#[test]
fn parallel_list_empty_returns_empty_list() {
    let src = r#"
import "std.flow" as flow
fn run0() -> List[Int] {
  flow.parallel_list([])
}
"#;
    let r = run(src, "run0", vec![]);
    assert_eq!(r, Value::List(vec![]));
}

#[test]
fn parallel_list_single_action_returns_singleton() {
    let src = r#"
import "std.flow" as flow
fn run1() -> List[Int] {
  flow.parallel_list([fn () -> Int { 42 }])
}
"#;
    let r = run(src, "run1", vec![]);
    assert_eq!(r, Value::List(vec![Value::Int(42)]));
}

#[test]
fn parallel_list_preserves_captured_locals() {
    // Each closure captures a distinct local from the enclosing scope.
    // Verifies captures are read at call time without aliasing.
    let src = r#"
import "std.flow" as flow
fn run_caps() -> List[Int] {
  let a := 1
  let b := 2
  let c := 3
  flow.parallel_list([
    fn () -> Int { a + 10 },
    fn () -> Int { b + 20 },
    fn () -> Int { c + 30 }
  ])
}
"#;
    let r = run(src, "run_caps", vec![]);
    assert_eq!(
        r,
        Value::List(vec![Value::Int(11), Value::Int(22), Value::Int(33)])
    );
}

#[test]
fn parallel_list_works_with_string_return_type() {
    let src = r#"
import "std.flow" as flow
fn run_strs() -> List[Str] {
  flow.parallel_list([
    fn () -> Str { "alpha" },
    fn () -> Str { "beta" },
    fn () -> Str { "gamma" }
  ])
}
"#;
    let r = run(src, "run_strs", vec![]);
    assert_eq!(
        r,
        Value::List(vec![
            Value::Str("alpha".into()),
            Value::Str("beta".into()),
            Value::Str("gamma".into()),
        ])
    );
}
