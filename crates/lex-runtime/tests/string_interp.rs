//! f"..." string interpolation (#562). Parse-time desugaring to str.concat chains.

use std::sync::Arc;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

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

fn s(v: &str) -> Value { Value::Str(v.into()) }

const PRELUDE: &str = "import \"std.str\" as str\n";

#[test]
fn plain_fstr_no_interpolation() {
    // f"..." with no braces is just a string literal.
    let src = format!("{PRELUDE}fn t() -> Str {{ f\"hello world\" }}\n");
    assert_eq!(run(&src, "t", vec![]), s("hello world"));
}

#[test]
fn single_var_interpolation() {
    let src = format!("{PRELUDE}fn greet(name :: Str) -> Str {{ f\"hello {{name}}!\" }}\n");
    assert_eq!(run(&src, "greet", vec![s("Alice")]), s("hello Alice!"));
}

#[test]
fn leading_expression() {
    // Interpolation at the start of the string, text after.
    let src = format!("{PRELUDE}fn t(x :: Str) -> Str {{ f\"{{x}} world\" }}\n");
    assert_eq!(run(&src, "t", vec![s("hello")]), s("hello world"));
}

#[test]
fn two_interpolations() {
    let src = format!("{PRELUDE}fn t(a :: Str, b :: Str) -> Str {{ f\"{{a}} and {{b}}\" }}\n");
    assert_eq!(run(&src, "t", vec![s("foo"), s("bar")]), s("foo and bar"));
}

#[test]
fn adjacent_interpolations() {
    let src = format!("{PRELUDE}fn t(a :: Str, b :: Str) -> Str {{ f\"{{a}}{{b}}\" }}\n");
    assert_eq!(run(&src, "t", vec![s("ab"), s("cd")]), s("abcd"));
}

#[test]
fn expression_in_braces() {
    // {str.concat(a, b)} — a call expression inside the braces.
    let src = format!("{PRELUDE}fn t(a :: Str, b :: Str) -> Str {{ f\"result: {{str.concat(a, b)}}\" }}\n");
    assert_eq!(run(&src, "t", vec![s("x"), s("y")]), s("result: xy"));
}
