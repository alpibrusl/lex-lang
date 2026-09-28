//! `todo()` (docs/design/project-to-issue-graph.md §3): compiles to the
//! same unconditional `Op::Panic` already emitted for a non-exhaustive
//! `match`, so reaching it at runtime aborts the call with `VmError::Panic`
//! rather than producing a bogus value.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Value, Vm, VmError};
use lex_syntax::parse_source;

fn compile(src: &str) -> lex_bytecode::Program {
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    compile_program(&stages)
}

#[test]
fn reaching_todo_panics_with_a_clear_message() {
    let p = compile("fn f() -> Int { todo() }\n");
    let mut vm = Vm::new(&p);
    let r = vm.call("f", vec![]);
    match r {
        Err(VmError::Panic(msg)) => assert!(msg.contains("todo"), "unexpected panic message: {msg}"),
        other => panic!("expected VmError::Panic, got {other:?}"),
    }
}

#[test]
fn todo_in_argument_position_panics_before_the_call() {
    let p = compile("fn callee(x :: Int) -> Int { x }\nfn f() -> Int { callee(todo()) }\n");
    let mut vm = Vm::new(&p);
    let r = vm.call("f", vec![]);
    assert!(matches!(r, Err(VmError::Panic(_))), "expected VmError::Panic, got {r:?}");
}

#[test]
fn a_branch_that_never_reaches_todo_runs_normally() {
    // Only the branch that's actually taken needs to be real; the other
    // arm's `todo()` never executes. This is the shape a partially
    // implemented issue in the skeleton looks like before every branch
    // has a real body.
    let src = r#"
fn f(done :: Bool) -> Int {
  match done {
    true => 1,
    false => todo(),
  }
}
"#;
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("f", vec![Value::Bool(true)]).unwrap();
    assert_eq!(r, Value::Int(1));
}

#[test]
fn user_defined_todo_runs_instead_of_panicking() {
    let p = compile("fn todo() -> Int { 42 }\nfn f() -> Int { todo() }\n");
    let mut vm = Vm::new(&p);
    let r = vm.call("f", vec![]).unwrap();
    assert_eq!(r, Value::Int(42));
}
