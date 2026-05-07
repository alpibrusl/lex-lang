//! Acceptance tests for #225: per-call budget runtime tracking.
//!
//! Pre-#225, `[budget(N)]` was enforced only by the static pre-flight
//! sum check in `policy::check_program`: it summed the declared budgets
//! across every function in the program and verified the total fit
//! under the ceiling. The runtime enforcement was a no-op (the
//! `("budget", _)` handler arm returned `Unit`). Net effect: any
//! function in a loop or called repeatedly could overspend the
//! declared budget without triggering anything.
//!
//! Now `Vm::Op::Call` / `Op::TailCall` / `Op::CallClosure` notify
//! the `EffectHandler` of the callee's declared `[budget(N)]` cost
//! via the new `note_call_budget` hook. `DefaultHandler` deducts
//! atomically from a shared `Arc<AtomicU64>` pool sized to the
//! policy ceiling, returning `BudgetExceeded` when a deduction
//! would underflow.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn compile_and_run(
    src: &str,
    fn_name: &str,
    args: Vec<Value>,
    policy: Policy,
) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).map_err(|e| format!("{e:?}"))
}

fn permissive_with_ceiling(ceiling: u64) -> Policy {
    let mut p = Policy::permissive();
    p.budget = Some(ceiling);
    p
}

const SRC: &str = r#"
fn step() -> [budget(10)] Int { 1 }

# Calls step twice. Static walk sees only one declaration of
# `[budget(10)]` at step's signature; runtime should deduct 10
# per invocation, totalling 20.
fn run_twice() -> [budget(10)] Int {
  let _a := step()
  let _b := step()
  42
}

fn run_once() -> [budget(10)] Int {
  step()
}
"#;

#[test]
fn single_call_under_ceiling_succeeds() {
    let v = compile_and_run(SRC, "run_once", vec![],
        permissive_with_ceiling(15)).unwrap();
    assert_eq!(v, Value::Int(1));
}

#[test]
fn repeated_calls_exceeding_ceiling_are_refused() {
    // Two `step()` calls cost 10 each = 20. Ceiling is 15; the
    // second call should trip the runtime check. Pre-#225 this
    // returned `Int(42)` — the bug we're closing.
    let err = compile_and_run(SRC, "run_twice", vec![],
        permissive_with_ceiling(15)).unwrap_err();
    assert!(err.contains("budget exceeded"),
        "expected budget-exceeded error, got: {err}");
}

#[test]
fn repeated_calls_under_ceiling_succeed() {
    // Two `step()` calls cost 20; ceiling 30 leaves room.
    let v = compile_and_run(SRC, "run_twice", vec![],
        permissive_with_ceiling(30)).unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn no_ceiling_means_no_runtime_check() {
    // `Policy::permissive()` without setting `budget` should not
    // enforce — the ceiling is `None`, the pool is `u64::MAX`,
    // and arbitrary repetition is allowed. Existing behavior must
    // be preserved.
    let p = Policy::permissive();
    let v = compile_and_run(SRC, "run_twice", vec![], p).unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn pure_function_calls_are_not_charged() {
    // A function with no declared `[budget(...)]` must not deduct
    // anything regardless of how many times it's called.
    let pure_src = r#"
fn helper() -> Int { 1 }
fn main_fn() -> Int {
  let _a := helper()
  let _b := helper()
  let _c := helper()
  helper()
}
"#;
    let v = compile_and_run(pure_src, "main_fn", vec![],
        permissive_with_ceiling(0)).unwrap();
    assert_eq!(v, Value::Int(1));
}

#[test]
fn budget_exceeded_error_names_the_ceiling() {
    let err = compile_and_run(SRC, "run_twice", vec![],
        permissive_with_ceiling(15)).unwrap_err();
    // The error string carries enough detail for an operator to
    // know what tripped without re-reading the trace.
    assert!(err.contains("ceiling 15"),
        "error should name the ceiling; got: {err}");
    assert!(err.contains("requested 10"),
        "error should name the offending request; got: {err}");
}
