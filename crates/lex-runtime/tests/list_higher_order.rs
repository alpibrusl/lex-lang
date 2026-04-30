//! Higher-order list operations: list.map, list.filter, list.fold.
//!
//! These compile to inline bytecode loops (not effect dispatch) so the
//! closure arg can flow through the VM's CallClosure opcode.

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

fn ints(xs: &[i64]) -> Value {
    Value::List(xs.iter().copied().map(Value::Int).collect())
}

#[test]
fn list_map_doubles() {
    let src = r#"
import "std.list" as list
fn doubled(xs :: List[Int]) -> List[Int] {
  list.map(xs, fn (n :: Int) -> Int { n * 2 })
}
"#;
    let r = run(src, "doubled", vec![ints(&[1, 2, 3, 4])]);
    assert_eq!(r, ints(&[2, 4, 6, 8]));
}

#[test]
fn list_map_on_empty_returns_empty() {
    let src = r#"
import "std.list" as list
fn ident(xs :: List[Int]) -> List[Int] {
  list.map(xs, fn (n :: Int) -> Int { n })
}
"#;
    assert_eq!(run(src, "ident", vec![ints(&[])]), ints(&[]));
}

#[test]
fn list_map_with_captured_local() {
    let src = r#"
import "std.list" as list
fn add_each(xs :: List[Int], k :: Int) -> List[Int] {
  list.map(xs, fn (n :: Int) -> Int { n + k })
}
"#;
    let r = run(src, "add_each", vec![ints(&[10, 20, 30]), Value::Int(5)]);
    assert_eq!(r, ints(&[15, 25, 35]));
}

#[test]
fn list_filter_keeps_matching() {
    let src = r#"
import "std.list" as list
fn evens(xs :: List[Int]) -> List[Int] {
  list.filter(xs, fn (n :: Int) -> Bool { (n % 2) == 0 })
}
"#;
    assert_eq!(run(src, "evens", vec![ints(&[1, 2, 3, 4, 5, 6])]), ints(&[2, 4, 6]));
}

#[test]
fn list_filter_all_pass_or_all_fail() {
    let src = r#"
import "std.list" as list
fn keep_pos(xs :: List[Int]) -> List[Int] {
  list.filter(xs, fn (n :: Int) -> Bool { n > 0 })
}
"#;
    assert_eq!(run(src, "keep_pos", vec![ints(&[1, 2, 3])]), ints(&[1, 2, 3]));
    assert_eq!(run(src, "keep_pos", vec![ints(&[-1, -2, -3])]), ints(&[]));
}

#[test]
fn list_fold_sums() {
    let src = r#"
import "std.list" as list
fn sum(xs :: List[Int]) -> Int {
  list.fold(xs, 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
}
"#;
    assert_eq!(run(src, "sum", vec![ints(&[])]), Value::Int(0));
    assert_eq!(run(src, "sum", vec![ints(&[1, 2, 3, 4, 5])]), Value::Int(15));
    assert_eq!(run(src, "sum", vec![ints(&[10])]), Value::Int(10));
}

#[test]
fn list_fold_builds_a_string() {
    let src = r#"
import "std.list" as list
import "std.str" as str
fn join_with_dash(xs :: List[Str]) -> Str {
  list.fold(xs, "", fn (acc :: Str, x :: Str) -> Str { str.concat(acc, x) })
}
"#;
    let xs = Value::List(vec![Value::Str("a".into()), Value::Str("b".into()), Value::Str("c".into())]);
    assert_eq!(run(src, "join_with_dash", vec![xs]), Value::Str("abc".into()));
}

#[test]
fn list_map_filter_fold_pipeline() {
    // pipeline: square evens, sum the squares
    let src = r#"
import "std.list" as list
fn sum_even_squares(xs :: List[Int]) -> Int {
  let evens := list.filter(xs, fn (n :: Int) -> Bool { (n % 2) == 0 })
  let squared := list.map(evens, fn (n :: Int) -> Int { n * n })
  list.fold(squared, 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
}
"#;
    // [1,2,3,4,5,6] → evens [2,4,6] → squared [4,16,36] → sum 56
    assert_eq!(run(src, "sum_even_squares", vec![ints(&[1, 2, 3, 4, 5, 6])]), Value::Int(56));
}

#[test]
fn list_map_then_fold_with_capture() {
    let src = r#"
import "std.list" as list
fn weighted_sum(xs :: List[Int], k :: Int) -> Int {
  let scaled := list.map(xs, fn (n :: Int) -> Int { n * k })
  list.fold(scaled, 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
}
"#;
    // sum([3*10, 4*10, 5*10]) = 120
    assert_eq!(run(src, "weighted_sum", vec![ints(&[3, 4, 5]), Value::Int(10)]), Value::Int(120));
}
