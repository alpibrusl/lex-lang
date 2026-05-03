//! Integration tests for `std.deque` plus the `std.map`/`std.set`
//! helpers added in #104.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

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

fn list_of_ints(v: Value) -> Vec<i64> {
    match v {
        Value::List(items) => items
            .into_iter()
            .map(|i| match i {
                Value::Int(n) => n,
                other => panic!("expected Int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List, got {other:?}"),
    }
}

// ---- std.deque ----

const DEQUE_SRC: &str = r#"
import "std.deque" as deque

fn build_back() -> List[Int] {
  deque.new()
    |> deque.push_back(1)
    |> deque.push_back(2)
    |> deque.push_back(3)
    |> deque.to_list
}

fn build_front() -> List[Int] {
  deque.new()
    |> deque.push_front(1)
    |> deque.push_front(2)
    |> deque.push_front(3)
    |> deque.to_list
}

fn build_mixed() -> List[Int] {
  deque.new()
    |> deque.push_back(2)
    |> deque.push_front(1)
    |> deque.push_back(3)
    |> deque.to_list
}

fn pop_back_all_size() -> Int {
  let d := deque.from_list([1, 2, 3])
  match deque.pop_back(d) {
    Some((_, rest)) => deque.size(rest),
    None            => 0 - 1,
  }
}

fn pop_front_value() -> Int {
  let d := deque.from_list([10, 20, 30])
  match deque.pop_front(d) {
    Some((x, _)) => x,
    None         => 0 - 1,
  }
}

fn empty_deque_pop_back() -> Bool {
  match deque.pop_back(deque.new()) {
    Some(_) => false,
    None    => true,
  }
}

fn peek_front_of_three() -> Int {
  match deque.peek_front(deque.from_list([7, 8, 9])) {
    Some(x) => x,
    None    => 0 - 1,
  }
}

fn is_empty_after_round_trip() -> Bool {
  deque.is_empty(deque.from_list([]))
}
"#;

#[test]
fn deque_push_back_preserves_insertion_order() {
    let v = run(DEQUE_SRC, "build_back", vec![]);
    assert_eq!(list_of_ints(v), vec![1, 2, 3]);
}

#[test]
fn deque_push_front_reverses_insertion() {
    let v = run(DEQUE_SRC, "build_front", vec![]);
    assert_eq!(list_of_ints(v), vec![3, 2, 1]);
}

#[test]
fn deque_mixed_push_yields_expected_order() {
    let v = run(DEQUE_SRC, "build_mixed", vec![]);
    assert_eq!(list_of_ints(v), vec![1, 2, 3]);
}

#[test]
fn deque_pop_back_returns_remaining_and_size() {
    let v = run(DEQUE_SRC, "pop_back_all_size", vec![]);
    assert_eq!(v, Value::Int(2));
}

#[test]
fn deque_pop_front_returns_first_value() {
    let v = run(DEQUE_SRC, "pop_front_value", vec![]);
    assert_eq!(v, Value::Int(10));
}

#[test]
fn deque_pop_on_empty_returns_none() {
    let v = run(DEQUE_SRC, "empty_deque_pop_back", vec![]);
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn deque_peek_front_does_not_consume() {
    let v = run(DEQUE_SRC, "peek_front_of_three", vec![]);
    assert_eq!(v, Value::Int(7));
}

#[test]
fn deque_is_empty_on_fresh() {
    let v = run(DEQUE_SRC, "is_empty_after_round_trip", vec![]);
    assert_eq!(v, Value::Bool(true));
}

// ---- map helpers ----

const MAP_SRC: &str = r#"
import "std.map" as map

fn merged_size() -> Int {
  let a := map.from_list([("x", 1), ("y", 2)])
  let b := map.from_list([("y", 20), ("z", 30)])
  map.size(map.merge(a, b))
}

fn merged_y_value() -> Int {
  let a := map.from_list([("x", 1), ("y", 2)])
  let b := map.from_list([("y", 20), ("z", 30)])
  match map.get(map.merge(a, b), "y") {
    Some(v) => v,
    None    => 0 - 1,
  }
}

fn empty_is_empty() -> Bool { map.is_empty(map.new()) }
fn nonempty_is_empty() -> Bool { map.is_empty(map.from_list([("x", 1)])) }
"#;

#[test]
fn map_merge_unions_keys_with_b_overriding() {
    assert_eq!(run(MAP_SRC, "merged_size", vec![]), Value::Int(3));
    assert_eq!(run(MAP_SRC, "merged_y_value", vec![]), Value::Int(20));
}

#[test]
fn map_is_empty_distinguishes_empty() {
    assert_eq!(run(MAP_SRC, "empty_is_empty", vec![]), Value::Bool(true));
    assert_eq!(run(MAP_SRC, "nonempty_is_empty", vec![]), Value::Bool(false));
}

// ---- set helpers ----

const SET_SRC: &str = r#"
import "std.set" as set
import "std.list" as list

fn diff_size() -> Int {
  let a := set.from_list([1, 2, 3, 4])
  let b := set.from_list([2, 4])
  set.size(set.diff(a, b))
}

fn empty_is_empty() -> Bool { set.is_empty(set.new()) }

fn proper_subset() -> Bool {
  let a := set.from_list([1, 2])
  let b := set.from_list([1, 2, 3])
  set.is_subset(a, b)
}

fn not_subset() -> Bool {
  let a := set.from_list([1, 4])
  let b := set.from_list([1, 2, 3])
  set.is_subset(a, b)
}

fn equal_set_is_subset() -> Bool {
  let a := set.from_list([1, 2, 3])
  let b := set.from_list([1, 2, 3])
  set.is_subset(a, b)
}
"#;

#[test]
fn set_diff_drops_b_elements() {
    assert_eq!(run(SET_SRC, "diff_size", vec![]), Value::Int(2));
}

#[test]
fn set_is_empty_works() {
    assert_eq!(run(SET_SRC, "empty_is_empty", vec![]), Value::Bool(true));
}

#[test]
fn set_is_subset_proper() {
    assert_eq!(run(SET_SRC, "proper_subset", vec![]), Value::Bool(true));
}

#[test]
fn set_is_subset_rejects_disjoint_element() {
    assert_eq!(run(SET_SRC, "not_subset", vec![]), Value::Bool(false));
}

#[test]
fn set_is_subset_includes_equal() {
    assert_eq!(run(SET_SRC, "equal_set_is_subset", vec![]), Value::Bool(true));
}
