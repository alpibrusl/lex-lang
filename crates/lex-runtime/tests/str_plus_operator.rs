//! #308: end-to-end coverage for `+` overloaded over Str.
//!
//! Pre-#308, `a + b` for `a, b :: Str` failed type-check with
//! `expected: "Int or Float"`. With #308 the type checker admits
//! Str+Str and the VM dispatches `Op::NumAdd` to string concat.
//! Other arithmetic ops (-, *, /, %) still reject Str so the
//! overload is intentionally minimal.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run_one(src: &str, entry: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    let policy = Policy::pure();
    check_program(&bc, &policy).expect("program type-checks under pure policy");
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(entry, args).unwrap()
}

#[test]
fn str_plus_str_concatenates() {
    let src = r#"
fn cat(a :: Str, b :: Str) -> Str { a + b }
"#;
    let r = run_one(
        src,
        "cat",
        vec![Value::Str("hello, ".into()), Value::Str("world".into())],
    );
    assert_eq!(r, Value::Str("hello, world".into()));
}

#[test]
fn str_plus_chains_left_to_right() {
    let src = r#"
fn three(a :: Str, b :: Str, c :: Str) -> Str { a + b + c }
"#;
    let r = run_one(
        src,
        "three",
        vec![
            Value::Str("foo".into()),
            Value::Str("-".into()),
            Value::Str("bar".into()),
        ],
    );
    assert_eq!(r, Value::Str("foo-bar".into()));
}

#[test]
fn int_plus_still_works() {
    // Regression: extending `+` to Str must not break numeric `+`.
    let src = r#"
fn add(a :: Int, b :: Int) -> Int { a + b }
"#;
    let r = run_one(src, "add", vec![Value::Int(2), Value::Int(40)]);
    assert_eq!(r, Value::Int(42));
}

#[test]
fn float_plus_still_works() {
    let src = r#"
fn addf(a :: Float, b :: Float) -> Float { a + b }
"#;
    let r = run_one(src, "addf", vec![Value::Float(1.5), Value::Float(2.25)]);
    assert_eq!(r, Value::Float(3.75));
}

fn type_errors(src: &str) -> Vec<lex_types::TypeError> {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    match lex_types::check_program(&stages) {
        Err(errs) => errs,
        Ok(_) => panic!("type check unexpectedly succeeded"),
    }
}

#[test]
fn str_minus_str_is_a_type_error() {
    // The overload is intentionally limited to `+`. Other arithmetic
    // ops on Str should still surface a TypeMismatch with the
    // "Int or Float" expectation, leaving the door open for a future
    // distinct semantics (e.g. strip-suffix) without forcing one now.
    let errs = type_errors(r#"
fn bad(a :: Str, b :: Str) -> Str { a - b }
"#);
    let msg = format!("{errs:?}");
    assert!(
        msg.contains("Int or Float"),
        "expected the numeric-only diagnostic, got: {msg}"
    );
}

#[test]
fn str_plus_int_is_a_type_error() {
    // Mixed Str + Int is rejected — no implicit coercion. The error
    // should still mention Str or Int so the diagnostic is useful.
    let errs = type_errors(r#"
fn bad(a :: Str, b :: Int) -> Str { a + b }
"#);
    let msg = format!("{errs:?}");
    assert!(
        msg.contains("Str") || msg.contains("Int"),
        "expected a useful diagnostic mentioning Str or Int, got: {msg}"
    );
}
