//! Acceptance tests for #209 slice 3: runtime residual checks for
//! refinement predicates that couldn't be discharged statically.
//!
//! Slice 2 catches literal-arg violations at compile time
//! (`withdraw(-5)` rejected before bytecode emission). Slice 3
//! catches the rest at the call boundary in the VM: any function
//! declaring `param :: Type{x | predicate}` evaluates the predicate
//! against the actual arg before pushing a frame. Failures raise
//! `VmError::RefinementFailed` with diagnostics naming the function,
//! parameter index, binding, and the offending value.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, vm::VmError, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn compile_and_call(src: &str, fn_name: &str, args: Vec<Value>) -> Result<Value, VmError> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::permissive())
        .with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args)
}

const POSITIVE_INT_SRC: &str = r#"
fn pos(amount :: Int{x | x > 0}) -> Int { amount }
"#;

#[test]
fn satisfying_runtime_arg_passes() {
    // Non-literal arg path: slice 2 defers, slice 3 checks at runtime.
    // Calling `pos(7)` from Rust skips the static-discharge layer
    // entirely (we hand the VM a `Value::Int(7)` directly), so this
    // exercises the slice-3 runtime evaluator.
    let v = compile_and_call(POSITIVE_INT_SRC, "pos", vec![Value::Int(7)])
        .expect("7 satisfies x > 0");
    assert_eq!(v, Value::Int(7));
}

#[test]
fn violating_runtime_arg_raises_refinement_failed() {
    // `pos(-3)` invoked at runtime — the bytecode VM evaluates the
    // refinement and rejects.
    let err = compile_and_call(POSITIVE_INT_SRC, "pos", vec![Value::Int(-3)])
        .expect_err("expected RefinementFailed");
    match err {
        VmError::RefinementFailed { fn_name, param_index, binding, reason } => {
            assert_eq!(fn_name, "pos");
            assert_eq!(param_index, 0);
            assert_eq!(binding, "x");
            assert!(reason.contains("-3"),
                "reason should name the failing value; got: {reason}");
        }
        other => panic!("expected RefinementFailed, got {other:?}"),
    }
}

#[test]
fn satisfying_arg_via_call_chain_passes() {
    // Caller passes a runtime-computed value that happens to satisfy
    // the refinement. The VM check fires during the inner call.
    let src = r#"
fn pos(amount :: Int{x | x > 0}) -> Int { amount }
fn add_one(n :: Int) -> Int { pos(n + 1) }
"#;
    let v = compile_and_call(src, "add_one", vec![Value::Int(5)])
        .expect("5 + 1 = 6 satisfies x > 0");
    assert_eq!(v, Value::Int(6));
}

#[test]
fn violating_arg_via_call_chain_raises() {
    // The runtime-computed value fails the predicate.
    let src = r#"
fn pos(amount :: Int{x | x > 0}) -> Int { amount }
fn maybe_negate(n :: Int) -> Int { pos(n - 100) }
"#;
    let err = compile_and_call(src, "maybe_negate", vec![Value::Int(5)])
        .expect_err("5 - 100 = -95 violates x > 0");
    assert!(matches!(err, VmError::RefinementFailed { .. }));
}

#[test]
fn compound_predicate_runtime_check_holds() {
    let src = r#"
fn bounded(x :: Int{n | n > 0 and n <= 100}) -> Int { x }
"#;
    let v = compile_and_call(src, "bounded", vec![Value::Int(50)])
        .expect("50 in (0, 100]");
    assert_eq!(v, Value::Int(50));
}

#[test]
fn compound_predicate_runtime_check_lower_bound_fails() {
    let src = r#"
fn bounded(x :: Int{n | n > 0 and n <= 100}) -> Int { x }
"#;
    let err = compile_and_call(src, "bounded", vec![Value::Int(0)])
        .expect_err("0 is not > 0");
    assert!(matches!(err, VmError::RefinementFailed { .. }));
}

#[test]
fn compound_predicate_runtime_check_upper_bound_fails() {
    let src = r#"
fn bounded(x :: Int{n | n > 0 and n <= 100}) -> Int { x }
"#;
    let err = compile_and_call(src, "bounded", vec![Value::Int(101)])
        .expect_err("101 is not <= 100");
    assert!(matches!(err, VmError::RefinementFailed { .. }));
}

#[test]
fn refinement_with_external_var_in_predicate_defers_to_runtime_error() {
    // Predicate references an unbound `balance` — slice 3's runtime
    // evaluator can't resolve it (just like slice 2 deferred). Surface
    // as RefinementFailed with a clear "free var" reason. Future work:
    // slice 4 plumbs call-site context bindings.
    let src = r#"
fn withdraw(amount :: Int{x | x > 0 and x <= balance}) -> Int { amount }
"#;
    let err = compile_and_call(src, "withdraw", vec![Value::Int(50)])
        .expect_err("predicate references free `balance`");
    match err {
        VmError::RefinementFailed { reason, .. } => {
            assert!(reason.contains("balance"),
                "reason should name the unresolved free var; got: {reason}");
        }
        other => panic!("expected RefinementFailed, got {other:?}"),
    }
}

#[test]
fn function_without_refinement_skips_check() {
    // Sanity: a function with no refined params works exactly as before;
    // the runtime check is a no-op when `refinements` is empty.
    let src = r#"
fn id(x :: Int) -> Int { x }
"#;
    let v = compile_and_call(src, "id", vec![Value::Int(-100)])
        .expect("no refinement → no check");
    assert_eq!(v, Value::Int(-100));
}
