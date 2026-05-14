//! Conformance for `list.sort_by` (#338) — stable sort over a
//! list by a closure-derived key. Keys can be `Int`, `Float`, or
//! `Str`; mixed-type or otherwise unorderable pairs are treated
//! as equal (the stable sort preserves their input order).

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, entry: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parses");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = lex_bytecode::compile_program(&stages);
    let handler = DefaultHandler::new(Policy::pure());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(entry, args).unwrap_or_else(|e| panic!("call {entry}: {e:?}"))
}

#[test]
fn sort_by_int_ascending() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Int]) -> List[Int] {
  list.sort_by(xs, fn(x :: Int) -> Int { x })
}
"#;
    let xs = Value::List(vec![
        Value::Int(3), Value::Int(1), Value::Int(4), Value::Int(1),
        Value::Int(5), Value::Int(9), Value::Int(2), Value::Int(6),
    ].into());
    let out = run(src, "r", vec![xs]);
    assert_eq!(
        out,
        Value::List(vec![
            Value::Int(1), Value::Int(1), Value::Int(2), Value::Int(3),
            Value::Int(4), Value::Int(5), Value::Int(6), Value::Int(9),
        ].into()),
    );
}

#[test]
fn sort_by_int_descending_via_negated_key() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Int]) -> List[Int] {
  list.sort_by(xs, fn(x :: Int) -> Int { 0 - x })
}
"#;
    let xs = Value::List(vec![Value::Int(2), Value::Int(7), Value::Int(1), Value::Int(4)].into());
    assert_eq!(
        run(src, "r", vec![xs]),
        Value::List(vec![Value::Int(7), Value::Int(4), Value::Int(2), Value::Int(1)].into()),
    );
}

#[test]
fn sort_by_str_lexicographic() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Str]) -> List[Str] {
  list.sort_by(xs, fn(s :: Str) -> Str { s })
}
"#;
    let xs = Value::List(vec![
        Value::Str("banana".into()),
        Value::Str("apple".into()),
        Value::Str("cherry".into()),
    ].into());
    assert_eq!(
        run(src, "r", vec![xs]),
        Value::List(vec![
            Value::Str("apple".into()),
            Value::Str("banana".into()),
            Value::Str("cherry".into()),
        ].into()),
    );
}

#[test]
fn sort_by_record_field() {
    // The canonical use case: sort a list of records by one field.
    let src = r#"
import "std.list" as list
type Order = { name :: Str, qty :: Int }
fn r(xs :: List[Order]) -> List[Order] {
  list.sort_by(xs, fn(o :: Order) -> Int { o.qty })
}
"#;
    let mk = |name: &str, qty: i64| {
        let mut fields = indexmap::IndexMap::new();
        fields.insert("name".into(), Value::Str(name.into()));
        fields.insert("qty".into(), Value::Int(qty));
        Value::Record(fields)
    };
    let xs = Value::List(vec![mk("bob", 5), mk("alice", 2), mk("carl", 3)].into());
    let Value::List(out) = run(src, "r", vec![xs]) else { panic!() };
    let qtys: Vec<i64> = out
        .iter()
        .map(|v| match v {
            Value::Record(f) => match f.get("qty") {
                Some(Value::Int(n)) => *n,
                _ => panic!(),
            },
            _ => panic!(),
        })
        .collect();
    assert_eq!(qtys, vec![2, 3, 5]);
}

#[test]
fn sort_by_is_stable_for_equal_keys() {
    // Stability: when two elements share a key, their relative
    // order in the input must survive the sort.
    let src = r#"
import "std.list" as list
type R = { k :: Int, tag :: Str }
fn r(xs :: List[R]) -> List[R] {
  list.sort_by(xs, fn(rec :: R) -> Int { rec.k })
}
"#;
    let mk = |k: i64, tag: &str| {
        let mut fields = indexmap::IndexMap::new();
        fields.insert("k".into(), Value::Int(k));
        fields.insert("tag".into(), Value::Str(tag.into()));
        Value::Record(fields)
    };
    let xs = Value::List(vec![
        mk(1, "a"), mk(2, "b"), mk(1, "c"), mk(2, "d"), mk(1, "e"),
    ].into());
    let Value::List(out) = run(src, "r", vec![xs]) else { panic!() };
    let tags: Vec<String> = out
        .iter()
        .map(|v| match v {
            Value::Record(f) => match f.get("tag") {
                Some(Value::Str(t)) => t.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        })
        .collect();
    // Original order for k=1 was a→c→e; for k=2 was b→d. Both
    // sub-orders must survive.
    assert_eq!(tags, vec!["a", "c", "e", "b", "d"]);
}

#[test]
fn sort_by_empty_and_singleton() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Int]) -> List[Int] {
  list.sort_by(xs, fn(x :: Int) -> Int { x })
}
"#;
    assert_eq!(run(src, "r", vec![Value::List(vec![].into())]), Value::List(vec![].into()));
    assert_eq!(
        run(src, "r", vec![Value::List(vec![Value::Int(42)].into())]),
        Value::List(vec![Value::Int(42)].into()),
    );
}

#[test]
fn sort_by_float_keys() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Float]) -> List[Float] {
  list.sort_by(xs, fn(x :: Float) -> Float { x })
}
"#;
    let xs = Value::List(vec![Value::Float(3.5), Value::Float(1.25), Value::Float(2.75)].into());
    assert_eq!(
        run(src, "r", vec![xs]),
        Value::List(vec![Value::Float(1.25), Value::Float(2.75), Value::Float(3.5)].into()),
    );
}
