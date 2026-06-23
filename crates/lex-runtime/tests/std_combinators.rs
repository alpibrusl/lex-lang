//! Combinator parity for `std.result` / `std.option` (#679).
//!
//! These functions had runtime support but were missing type
//! signatures (so they couldn't be called from Lex source), or were
//! missing entirely on one of the two types. This pins:
//!   - result.unwrap_or / unwrap_or_else / is_ok / is_err
//!   - option.is_some / is_none / ok_or

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.result" as result
import "std.option" as option
import "std.str"    as str

fn r_unwrap_or(r :: Result[Int, Str], d :: Int) -> Int { result.unwrap_or(r, d) }

# Lazy fallback receives the Err payload — here its length.
fn r_unwrap_or_else(r :: Result[Int, Str]) -> Int {
  result.unwrap_or_else(r, fn (e :: Str) -> Int { str.len(e) })
}

fn r_is_ok(r :: Result[Int, Str]) -> Bool { result.is_ok(r) }
fn r_is_err(r :: Result[Int, Str]) -> Bool { result.is_err(r) }

fn o_is_some(o :: Option[Int]) -> Bool { option.is_some(o) }
fn o_is_none(o :: Option[Int]) -> Bool { option.is_none(o) }
fn o_ok_or(o :: Option[Int], e :: Str) -> Result[Int, Str] { option.ok_or(o, e) }
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

fn ok(v: Value) -> Value { Value::Variant { name: "Ok".into(), args: vec![v] } }
fn err(v: Value) -> Value { Value::Variant { name: "Err".into(), args: vec![v] } }
fn some(v: Value) -> Value { Value::Variant { name: "Some".into(), args: vec![v] } }
fn none() -> Value { Value::Variant { name: "None".into(), args: vec![] } }

#[test]
fn result_unwrap_or() {
    assert_eq!(run("r_unwrap_or", vec![ok(Value::Int(7)), Value::Int(99)]), Value::Int(7));
    assert_eq!(run("r_unwrap_or", vec![err(Value::Str("x".into())), Value::Int(99)]), Value::Int(99));
}

#[test]
fn result_unwrap_or_else_forwards_err_payload() {
    // Ok → unwrapped value; the closure must not run.
    assert_eq!(run("r_unwrap_or_else", vec![ok(Value::Int(7))]), Value::Int(7));
    // Err → closure runs on the payload "boom" → length 4.
    assert_eq!(run("r_unwrap_or_else", vec![err(Value::Str("boom".into()))]), Value::Int(4));
}

#[test]
fn result_is_ok_is_err() {
    assert_eq!(run("r_is_ok", vec![ok(Value::Int(1))]), Value::Bool(true));
    assert_eq!(run("r_is_ok", vec![err(Value::Str("e".into()))]), Value::Bool(false));
    assert_eq!(run("r_is_err", vec![ok(Value::Int(1))]), Value::Bool(false));
    assert_eq!(run("r_is_err", vec![err(Value::Str("e".into()))]), Value::Bool(true));
}

#[test]
fn option_is_some_is_none() {
    assert_eq!(run("o_is_some", vec![some(Value::Int(3))]), Value::Bool(true));
    assert_eq!(run("o_is_some", vec![none()]), Value::Bool(false));
    assert_eq!(run("o_is_none", vec![some(Value::Int(3))]), Value::Bool(false));
    assert_eq!(run("o_is_none", vec![none()]), Value::Bool(true));
}

#[test]
fn option_ok_or_crosses_into_result() {
    assert_eq!(run("o_ok_or", vec![some(Value::Int(3)), Value::Str("e".into())]), ok(Value::Int(3)));
    assert_eq!(run("o_ok_or", vec![none(), Value::Str("e".into())]), err(Value::Str("e".into())));
}
