//! Integration tests for `std.iter` (#364) — Iter[T] lazy positional iterator.
//! Backed internally by (List[T], Int); all operations are compiler-inlined.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let mut vm = Vm::with_handler(&bc, Box::new(DefaultHandler::new(Policy::pure())));
    vm.call(func, args).expect("vm")
}

const SRC: &str = r#"
import "std.iter" as iter
import "std.list" as list

fn from_list_to_list(xs :: List[Int]) -> List[Int] {
  iter.to_list(iter.from_list(xs))
}

fn next_on_empty() -> Bool {
  let it := iter.from_list([])
  match iter.next(it) {
    None         => true,
    Some(_)      => false,
  }
}

fn next_first_elem(xs :: List[Int]) -> Option[Int] {
  match iter.next(iter.from_list(xs)) {
    None              => None,
    Some((x, _rest))  => Some(x),
  }
}

fn take_two(xs :: List[Int]) -> List[Int] {
  iter.to_list(iter.take(iter.from_list(xs), 2))
}

fn skip_two(xs :: List[Int]) -> List[Int] {
  iter.to_list(iter.skip(iter.from_list(xs), 2))
}

fn is_empty_after_take_all(xs :: List[Int]) -> Bool {
  iter.is_empty(iter.skip(iter.from_list(xs), list.len(xs)))
}

fn count_remaining(xs :: List[Int]) -> Int {
  iter.count(iter.from_list(xs))
}

fn map_double(xs :: List[Int]) -> List[Int] {
  iter.to_list(iter.map(iter.from_list(xs), fn (x :: Int) -> Int { x * 2 }))
}

fn filter_even(xs :: List[Int]) -> List[Int] {
  iter.to_list(iter.filter(iter.from_list(xs), fn (x :: Int) -> Bool { x - (x / 2) * 2 == 0 }))
}

fn fold_sum(xs :: List[Int]) -> Int {
  iter.fold(iter.from_list(xs), 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
}

fn chained_skip_take(xs :: List[Int]) -> List[Int] {
  iter.to_list(iter.take(iter.skip(iter.from_list(xs), 1), 2))
}
"#;

#[test]
fn from_list_and_to_list_roundtrip() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    let got = run(SRC, "from_list_to_list", vec![xs.clone()]);
    assert_eq!(got, xs);
}

#[test]
fn next_on_empty_iter_is_none() {
    let got = run(SRC, "next_on_empty", vec![]);
    assert_eq!(got, Value::Bool(true));
}

#[test]
fn next_returns_first_element() {
    let xs = Value::List(vec![Value::Int(10), Value::Int(20)]);
    let got = run(SRC, "next_first_elem", vec![xs]);
    assert_eq!(got, Value::Variant { name: "Some".into(), args: vec![Value::Int(10)] });
}

#[test]
fn take_limits_to_n_elements() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]);
    let got = run(SRC, "take_two", vec![xs]);
    assert_eq!(got, Value::List(vec![Value::Int(1), Value::Int(2)]));
}

#[test]
fn take_beyond_length_returns_all() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2)]);
    let got = run(SRC, "take_two", vec![xs.clone()]);
    assert_eq!(got, xs);
}

#[test]
fn skip_advances_cursor() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]);
    let got = run(SRC, "skip_two", vec![xs]);
    assert_eq!(got, Value::List(vec![Value::Int(3), Value::Int(4)]));
}

#[test]
fn skip_beyond_length_gives_empty() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2)]);
    let got = run(SRC, "skip_two", vec![xs]);
    assert_eq!(got, Value::List(vec![]));
}

#[test]
fn is_empty_after_exhaustion() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2)]);
    let got = run(SRC, "is_empty_after_take_all", vec![xs]);
    assert_eq!(got, Value::Bool(true));
}

#[test]
fn count_returns_remaining_size() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    let got = run(SRC, "count_remaining", vec![xs]);
    assert_eq!(got, Value::Int(3));
}

#[test]
fn map_doubles_elements() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    let got = run(SRC, "map_double", vec![xs]);
    assert_eq!(got, Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)]));
}

#[test]
fn filter_keeps_even_numbers() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]);
    let got = run(SRC, "filter_even", vec![xs]);
    assert_eq!(got, Value::List(vec![Value::Int(2), Value::Int(4)]));
}

#[test]
fn fold_sums_elements() {
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)]);
    let got = run(SRC, "fold_sum", vec![xs]);
    assert_eq!(got, Value::Int(10));
}

#[test]
fn chained_skip_and_take() {
    let xs = Value::List(vec![
        Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4), Value::Int(5),
    ]);
    let got = run(SRC, "chained_skip_take", vec![xs]);
    assert_eq!(got, Value::List(vec![Value::Int(2), Value::Int(3)]));
}
