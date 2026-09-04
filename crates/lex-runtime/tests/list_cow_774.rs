//! #774: lists are shared copy-on-write, and a local's last read is a
//! move, so accumulating a list element by element is O(n).
//!
//! Before: `Value::List` held its `VecDeque` by value, the VM cloned a
//! local onto the operand stack for every read, and `list.cons` got a
//! deep copy of the accumulator on every call. 16,000 elements took
//! 8.9 s in a debug build.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::time::{Duration, Instant};

fn run(src: &str, func: &str, args: Vec<Value>) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return Err(format!("type errors: {errs:#?}"));
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.set_step_limit(u64::MAX);
    vm.call(func, args).map_err(|e| format!("{e}"))
}

const SRC: &str = r#"
import "std.list" as list

# fold + cons: the accumulator is a closure argument.
fn cons_fold(n :: Int) -> Int {
  list.len(list.fold(list.range(0, n), [], fn (acc :: List[Int], i :: Int) -> List[Int] {
    list.cons(i, acc)
  }))
}

# tail recursion threading the accumulator through the call.
fn build(n :: Int, acc :: List[Int]) -> List[Int] {
  if n <= 0 { acc } else { build(n - 1, list.cons(n, acc)) }
}
fn cons_tail(n :: Int) -> Int { list.len(build(n, [])) }

# the `let`-then-call shape a parser loop uses.
fn build_let(n :: Int, acc :: List[Int]) -> List[Int] {
  if n <= 0 {
    acc
  } else {
    let acc2 := list.cons(n, acc)
    build_let(n - 1, acc2)
  }
}
fn cons_let(n :: Int) -> Int { list.len(build_let(n, [])) }

# --- semantics: a moved-out read must be the last one ---------------

# xs is read three times; only the last read may move.
fn reads_after_cons(xs :: List[Int]) -> Int {
  let a := list.len(xs)
  let b := list.len(list.cons(1, xs))
  a * 1000000 + b * 1000 + list.len(xs)
}

# both branches read xs; the earlier-emitted arm keeps a clone.
fn branch_reads(xs :: List[Int], c :: Bool) -> Int {
  if c { list.len(xs) } else { list.len(list.cons(0, xs)) }
}

# xs is shared with ys at mutation time: cons must copy, not alias.
fn cow_keeps_original(xs :: List[Int]) -> Int {
  let ys := list.cons(1, xs)
  list.len(xs) * 1000 + list.len(ys)
}

# a lambda captures xs, then the body reads it again.
fn capture_then_read(xs :: List[Int]) -> Int {
  let f := fn (i :: Int) -> Int { i + list.len(xs) }
  f(1) * 1000 + list.len(xs)
}

# the same slot read in both `match` arms of a nested expression.
fn match_reads(xs :: List[Int], k :: Int) -> Int {
  let pick := match k {
    0 => list.len(xs),
    _ => list.len(list.cons(k, xs)),
  }
  pick * 1000 + list.len(xs)
}
"#;

fn ints(n: i64) -> Value {
    Value::List((0..n).map(Value::Int).collect())
}

#[test]
fn cons_in_a_fold_is_linear() {
    let n = 40_000;
    let t = Instant::now();
    let v = run(SRC, "cons_fold", vec![Value::Int(n)]).unwrap();
    assert_eq!(v, Value::Int(n));
    // Quadratic accumulation took minutes at this size in a debug build.
    assert!(t.elapsed() < Duration::from_secs(20), "cons_fold took {:?}", t.elapsed());
}

#[test]
fn cons_through_a_tail_call_is_linear() {
    let n = 40_000;
    let t = Instant::now();
    let v = run(SRC, "cons_tail", vec![Value::Int(n)]).unwrap();
    assert_eq!(v, Value::Int(n));
    assert!(t.elapsed() < Duration::from_secs(20), "cons_tail took {:?}", t.elapsed());
}

#[test]
fn cons_through_a_let_is_linear() {
    let n = 40_000;
    let t = Instant::now();
    let v = run(SRC, "cons_let", vec![Value::Int(n)]).unwrap();
    assert_eq!(v, Value::Int(n));
    assert!(t.elapsed() < Duration::from_secs(20), "cons_let took {:?}", t.elapsed());
}

#[test]
fn earlier_reads_still_see_the_value() {
    let v = run(SRC, "reads_after_cons", vec![ints(5)]).unwrap();
    assert_eq!(v, Value::Int(5 * 1_000_000 + 6 * 1000 + 5));
}

#[test]
fn both_branches_read_correctly() {
    assert_eq!(run(SRC, "branch_reads", vec![ints(3), Value::Bool(true)]).unwrap(), Value::Int(3));
    assert_eq!(run(SRC, "branch_reads", vec![ints(3), Value::Bool(false)]).unwrap(), Value::Int(4));
}

#[test]
fn a_shared_list_is_copied_on_write() {
    let v = run(SRC, "cow_keeps_original", vec![ints(4)]).unwrap();
    assert_eq!(v, Value::Int(4 * 1000 + 5));
}

#[test]
fn capture_then_read_sees_the_value() {
    let v = run(SRC, "capture_then_read", vec![ints(7)]).unwrap();
    assert_eq!(v, Value::Int((1 + 7) * 1000 + 7));
}

#[test]
fn match_arms_read_correctly() {
    assert_eq!(run(SRC, "match_reads", vec![ints(2), Value::Int(0)]).unwrap(), Value::Int(2 * 1000 + 2));
    assert_eq!(run(SRC, "match_reads", vec![ints(2), Value::Int(9)]).unwrap(), Value::Int(3 * 1000 + 2));
}

#[test]
fn list_clone_is_shared_until_written() {
    let a = lex_bytecode::List::from(vec![Value::Int(1), Value::Int(2)]);
    let mut b = a.clone();
    assert!(!a.is_unique());
    b.push_back(Value::Int(3));
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 3);
    assert!(a.is_unique());
    assert!(b.is_unique());
}
