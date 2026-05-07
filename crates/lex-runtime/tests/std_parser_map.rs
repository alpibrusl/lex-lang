//! Integration tests for `parser.map` and `parser.and_then` (#221).
//!
//! These pin the closure-bearing combinators that were carved out
//! of #217's v1 because their closures broke the canonical-parsers
//! property — a constraint that #222's content-addressed closure
//! identities lifted. The interpreter side (closure invocation
//! during the recursive parse) lives in `lex-bytecode::parser_runtime`
//! and is reached via the Vm-level `parser.run` intercept.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn compile_and_handler(src: &str) -> (Arc<lex_bytecode::Program>, DefaultHandler) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    (bc, handler)
}

fn call(src: &str, name: &str, args: Vec<Value>) -> Value {
    let (bc, handler) = compile_and_handler(src);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(name, args).unwrap_or_else(|e| panic!("call {name}: {e}"))
}

fn variant_name(v: &Value) -> &str {
    match v {
        Value::Variant { name, .. } => name.as_str(),
        other => panic!("expected Variant, got {other:?}"),
    }
}

// --------------------------------------------------------------- parser.map

const MAP_SRC: &str = r#"
import "std.parser" as p
import "std.str" as str

# Parse a digit and convert to a tag string. Demonstrates the
# transform-the-value contract.
fn digit_to_label(s :: Str) -> Result[Str, { pos :: Int, message :: Str }] {
  let parser := p.map(p.digit(),
    fn (d :: Str) -> Str { str.concat("digit:", d) })
  p.run(parser, s)
}

# map should compose: applying map twice produces the doubly-transformed value.
fn double_map(s :: Str) -> Result[Str, { pos :: Int, message :: Str }] {
  let inner := p.map(p.digit(),
    fn (d :: Str) -> Str { str.concat("d=", d) })
  let outer := p.map(inner,
    fn (s :: Str) -> Str { str.concat("[", str.concat(s, "]")) })
  p.run(outer, s)
}

# map propagating failure — closure should not run when inner parser fails.
fn map_failure_skips_closure(s :: Str) -> Result[Str, { pos :: Int, message :: Str }] {
  let parser := p.map(p.digit(),
    fn (_d :: Str) -> Str { "should-not-appear" })
  p.run(parser, s)
}
"#;

#[test]
fn map_transforms_parsed_value() {
    let v = call(MAP_SRC, "digit_to_label", vec![Value::Str("7".into())]);
    let (name, args) = match v {
        Value::Variant { name, args } => (name, args),
        other => panic!("{other:?}"),
    };
    assert_eq!(name, "Ok");
    match args.first() {
        Some(Value::Str(s)) => assert_eq!(s, "digit:7"),
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn map_composes() {
    let v = call(MAP_SRC, "double_map", vec![Value::Str("3".into())]);
    let (name, args) = match v {
        Value::Variant { name, args } => (name, args),
        other => panic!("{other:?}"),
    };
    assert_eq!(name, "Ok");
    match args.first() {
        Some(Value::Str(s)) => assert_eq!(s, "[d=3]"),
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn map_failure_propagates_without_running_closure() {
    // The inner digit() fails on "x"; the closure must not fire.
    let v = call(MAP_SRC, "map_failure_skips_closure", vec![Value::Str("x".into())]);
    assert_eq!(variant_name(&v), "Err");
}

// ---------------------------------------------------------- parser.and_then

const AND_THEN_SRC: &str = r#"
import "std.parser" as p
import "std.str" as str

# Read a digit; if it's "1", expect "ONE" next; otherwise expect
# "OTHER". Monadic bind: the second parser depends on the first
# parsed value.
fn dispatch_on_digit(s :: Str) -> Result[(Str, Str), { pos :: Int, message :: Str }] {
  let parser := p.and_then(p.digit(),
    fn (d :: Str) -> Parser[Str] {
      match d == "1" {
        true => p.string("ONE"),
        false => p.string("OTHER"),
      }
    })
  # Pair the original digit with the second-stage match. We can't
  # express this cleanly without map, so we use seq + and_then
  # nested: run digit, branch on it, return the branch result. The
  # tuple shape comes from threading both halves through seq.
  p.run(p.seq(p.digit(), parser), s)
}
"#;

#[test]
fn and_then_dispatches_on_parsed_value_one_branch() {
    let v = call(AND_THEN_SRC, "dispatch_on_digit", vec![Value::Str("11ONE".into())]);
    let (name, args) = match v {
        Value::Variant { name, args } => (name, args),
        other => panic!("{other:?}"),
    };
    assert_eq!(name, "Ok");
    if let Some(Value::Tuple(parts)) = args.first() {
        assert_eq!(parts.len(), 2);
    } else {
        panic!("expected Tuple, got {args:?}");
    }
}

#[test]
fn and_then_dispatches_on_parsed_value_other_branch() {
    let v = call(AND_THEN_SRC, "dispatch_on_digit", vec![Value::Str("22OTHER".into())]);
    assert_eq!(variant_name(&v), "Ok");
}

#[test]
fn and_then_branch_failure_propagates() {
    // "12" picks the "ONE" branch, which expects "ONE" but gets "2".
    let v = call(AND_THEN_SRC, "dispatch_on_digit", vec![Value::Str("12abc".into())]);
    assert_eq!(variant_name(&v), "Err");
}

// ------------------------------------------------------- canonical equality

const CANON_SRC: &str = r#"
import "std.parser" as p
import "std.str" as str

# Two parsers built by structurally equivalent code paths should
# produce equal Values. With #222's body-hash-based closure equality,
# this property holds even for closure-bearing combinators (#221).
fn build_a() -> Parser[Str] {
  p.map(p.digit(), fn (d :: Str) -> Str { str.concat("d=", d) })
}
fn build_b() -> Parser[Str] {
  p.map(p.digit(), fn (d :: Str) -> Str { str.concat("d=", d) })
}
"#;

#[test]
fn equivalent_map_parsers_compare_equal() {
    let a = call(CANON_SRC, "build_a", vec![]);
    let b = call(CANON_SRC, "build_b", vec![]);
    assert_eq!(a, b,
        "two parser.map(digit, fn(d) -> ...) calls with identical \
         closure bodies should produce equal parser values — the \
         #222 canonicality property must apply to closure-bearing \
         combinators too");
}
