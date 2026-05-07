//! Acceptance test for #222: content-addressed closure identity.
//!
//! Two closure literals with identical bodies but at different source
//! locations must compare equal as `Value`. The mechanism is the
//! `body_hash` field on `Value::Closure`; `fn_id` differs between the
//! two literals but is excluded from `PartialEq`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
# Two functions returning the *same* closure literal `fn(x) -> x + 1`
# from two distinct source locations. Pre-#222 the resulting closures
# would have different fn_ids and compare unequal; post-#222 they
# should compare equal because their body hashes coincide.

fn make_a() -> (Int) -> Int { fn (x :: Int) -> Int { x + 1 } }
fn make_b() -> (Int) -> Int { fn (x :: Int) -> Int { x + 1 } }

# A closure that captures a local. Equality should still hold across
# source locations when the captures match.
fn make_capturing_a(n :: Int) -> (Int) -> Int {
  fn (x :: Int) -> Int { x + n }
}
fn make_capturing_b(n :: Int) -> (Int) -> Int {
  fn (x :: Int) -> Int { x + n }
}

# A closure with a *different* body — must NOT compare equal to the
# first two even though the shape is similar.
fn make_different() -> (Int) -> Int { fn (x :: Int) -> Int { x + 2 } }
"#;

struct DenyAll;
impl lex_bytecode::vm::EffectHandler for DenyAll {
    fn dispatch(&mut self, kind: &str, op: &str, _args: Vec<Value>) -> Result<Value, String> {
        Err(format!("effect {kind}.{op} not permitted"))
    }
}

fn call(name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut vm = Vm::with_handler(&bc, Box::new(DenyAll));
    vm.call(name, args).unwrap_or_else(|e| panic!("call {name}: {e}"))
}

#[test]
fn equal_bodies_at_different_locations_compare_equal() {
    let a = call("make_a", vec![]);
    let b = call("make_b", vec![]);
    assert_eq!(a, b,
        "closures with identical bodies should be equal even when \
         their fn_ids differ — got {a:?} vs {b:?}");
}

#[test]
fn equal_bodies_with_equal_captures_compare_equal() {
    let a = call("make_capturing_a", vec![Value::Int(7)]);
    let b = call("make_capturing_b", vec![Value::Int(7)]);
    assert_eq!(a, b);
}

#[test]
fn equal_bodies_with_different_captures_compare_unequal() {
    let a = call("make_capturing_a", vec![Value::Int(7)]);
    let b = call("make_capturing_b", vec![Value::Int(8)]);
    assert_ne!(a, b,
        "captures of (7) and (8) should produce non-equal closures");
}

#[test]
fn different_bodies_compare_unequal() {
    let a = call("make_a", vec![]);
    let c = call("make_different", vec![]);
    assert_ne!(a, c,
        "fn(x) -> x + 1 and fn(x) -> x + 2 must not compare equal");
}

#[test]
fn body_hash_field_is_populated() {
    // Catches a regression where the final hash pass is skipped and
    // closures end up with the all-zero sentinel.
    let v = call("make_a", vec![]);
    match v {
        Value::Closure { body_hash, .. } => {
            assert_ne!(body_hash, [0u8; 16],
                "Value::Closure body_hash must not be zero — the \
                 final hash pass in compile_program didn't run");
        }
        other => panic!("expected Closure, got {other:?}"),
    }
}
