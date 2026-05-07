//! Integration test for `std.option.and_then`.
//!
//! The compiler entry has been wired in `lex-bytecode` since the
//! variant-map work landed, but the type-check signature was missing
//! from `lex-types::builtins`, so calling `option.and_then` from a
//! Lex program failed before runtime. This pins the fix.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.option" as opt
import "std.str"    as str

# Parse `s` as Int, then double it. Composes two Option-returning
# steps via and_then — the historical type-check failure mode.
fn parse_then_double(s :: Str) -> Option[Int] {
  opt.and_then(str.to_int(s),
    fn (n :: Int) -> Option[Int] { Some(n + n) })
}

# Short-circuit on None: closure must not run.
fn none_short_circuits() -> Option[Int] {
  opt.and_then(None,
    fn (n :: Int) -> Option[Int] { Some(n + 1) })
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

fn variant_name(v: &Value) -> &str {
    match v {
        Value::Variant { name, .. } => name.as_str(),
        other => panic!("expected Variant, got {other:?}"),
    }
}

#[test]
fn and_then_chains_some() {
    let v = run("parse_then_double", vec![Value::Str("21".into())]);
    assert_eq!(variant_name(&v), "Some");
    if let Value::Variant { args, .. } = &v {
        assert_eq!(args.first(), Some(&Value::Int(42)));
    }
}

#[test]
fn and_then_propagates_none_from_first_step() {
    // `str.to_int("notanumber")` returns None; the closure must not run.
    let v = run("parse_then_double", vec![Value::Str("notanumber".into())]);
    assert_eq!(variant_name(&v), "None");
}

#[test]
fn and_then_starting_from_none_short_circuits() {
    let v = run("none_short_circuits", vec![]);
    assert_eq!(variant_name(&v), "None");
}
