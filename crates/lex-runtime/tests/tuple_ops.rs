//! std.tuple — fst, snd, third, len. Per spec §11.1.
//!
//! `Value::Tuple` already exists in the VM; these are pure builtins
//! that index into it. Tests both that the type signatures unify
//! cleanly across heterogeneous element types and that the runtime
//! returns the right value.

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
fn tuple_fst_returns_first_element() {
    let src = r#"
import "std.tuple" as tuple
fn first(p :: Tuple[Int, Str]) -> Int { tuple.fst(p) }
"#;
    let r = run(src, "first", vec![Value::Tuple(vec![
        Value::Int(7),
        Value::Str("ignored".into()),
    ])]);
    assert_eq!(r, Value::Int(7));
}

#[test]
fn tuple_snd_returns_second_element() {
    let src = r#"
import "std.tuple" as tuple
fn second(p :: Tuple[Int, Str]) -> Str { tuple.snd(p) }
"#;
    let r = run(src, "second", vec![Value::Tuple(vec![
        Value::Int(0),
        Value::Str("hello".into()),
    ])]);
    assert_eq!(r, Value::Str("hello".into()));
}

#[test]
fn tuple_third_returns_third_element() {
    let src = r#"
import "std.tuple" as tuple
fn third(p :: Tuple[Int, Str, Bool]) -> Bool { tuple.third(p) }
"#;
    let r = run(src, "third", vec![Value::Tuple(vec![
        Value::Int(1),
        Value::Str("two".into()),
        Value::Bool(true),
    ])]);
    assert_eq!(r, Value::Bool(true));
}

#[test]
fn tuple_len_counts_pair() {
    let src = r#"
import "std.tuple" as tuple
fn n(p :: Tuple[Int, Str]) -> Int { tuple.len(p) }
"#;
    let r = run(src, "n", vec![Value::Tuple(vec![
        Value::Int(1),
        Value::Str("two".into()),
    ])]);
    assert_eq!(r, Value::Int(2));
}
