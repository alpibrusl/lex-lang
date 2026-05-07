//! Acceptance test for #222 from the runtime side: closure-bearing
//! HOFs (here `flow.sequential`) compose canonically.
//!
//! Two `flow.sequential(f, g)` constructions at different source
//! locations must produce equal `Value`s when `f` and `g` are equal.
//! Pre-#222 the resulting `Value::Closure` trampolines had distinct
//! `fn_id`s — same body, different identities. Post-#222 the body
//! hash matches and equality holds.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.flow" as flow

fn build_a() -> (Int) -> Int {
  flow.sequential(
    fn (x :: Int) -> Int { x + 1 },
    fn (y :: Int) -> Int { y * 2 })
}

fn build_b() -> (Int) -> Int {
  flow.sequential(
    fn (x :: Int) -> Int { x + 1 },
    fn (y :: Int) -> Int { y * 2 })
}

# Different inner functions — must NOT compare equal.
fn build_different() -> (Int) -> Int {
  flow.sequential(
    fn (x :: Int) -> Int { x + 1 },
    fn (y :: Int) -> Int { y * 3 })
}
"#;

fn call(name: &str) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(name, vec![]).unwrap_or_else(|e| panic!("call {name}: {e}"))
}

#[test]
fn flow_sequential_at_different_sites_compares_equal() {
    let a = call("build_a");
    let b = call("build_b");
    assert_eq!(a, b,
        "two flow.sequential(f, g) values built from equal f, g \
         at different source locations must compare equal — got \
         {a:?} vs {b:?}");
}

#[test]
fn flow_sequential_with_different_inner_fn_compares_unequal() {
    let a = call("build_a");
    let c = call("build_different");
    assert_ne!(a, c,
        "different inner closures must produce distinct flow values");
}
