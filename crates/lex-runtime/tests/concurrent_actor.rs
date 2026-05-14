//! End-to-end tests for #381: conc.spawn / conc.ask / conc.tell.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::VecDeque;

fn run(src: &str, entry: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    let policy = Policy::permissive();
    check_program(&bc, &policy).expect("policy check");
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(entry, args).unwrap()
}

// ── basic counter actor ──────────────────────────────────────────────────────

#[test]
fn actor_counter_ask_increments_state() {
    let src = r#"
import "std.conc" as conc

fn counter_handler(state :: Int, msg :: Int) -> (Int, Int) {
  let next := state + msg
  (next, next)
}

fn make_and_use() -> Int {
  let actor := conc.spawn(0, counter_handler)
  let _     := conc.ask(actor, 5)
  let r     := conc.ask(actor, 3)
  r
}
"#;
    assert_eq!(run(src, "make_and_use", vec![]), Value::Int(8));
}

// ── tell discards reply ──────────────────────────────────────────────────────

#[test]
fn actor_tell_returns_unit() {
    let src = r#"
import "std.conc" as conc

fn acc_int_handler(state :: Int, msg :: Int) -> (Int, Int) {
  let next := state + msg
  (next, next)
}

fn test_tell() -> Int {
  let actor := conc.spawn(0, acc_int_handler)
  let _     := conc.tell(actor, 10)
  let _     := conc.tell(actor, 10)
  conc.ask(actor, 0)
}
"#;
    // After two tell(10) calls state is 20; ask(0) → new state 20, reply 20.
    assert_eq!(run(src, "test_tell", vec![]), Value::Int(20));
}

// ── actor state persists across calls ───────────────────────────────────────

#[test]
fn actor_state_accumulates() {
    let src = r#"
import "std.conc" as conc
import "std.list" as list

fn acc_handler(state :: List[Int], msg :: Int) -> (List[Int], List[Int]) {
  let next := list.concat(state, list.cons(msg, []))
  (next, next)
}

fn collect_items() -> List[Int] {
  let actor := conc.spawn([], acc_handler)
  let _ := conc.tell(actor, 1)
  let _ := conc.tell(actor, 2)
  let _ := conc.tell(actor, 3)
  conc.ask(actor, 4)
}
"#;
    let result = run(src, "collect_items", vec![]);
    assert_eq!(
        result,
        Value::List(VecDeque::from(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
        ]))
    );
}

// ── multiple independent actors ──────────────────────────────────────────────

#[test]
fn two_independent_actors_have_separate_state() {
    let src = r#"
import "std.conc" as conc

fn counter(state :: Int, msg :: Int) -> (Int, Int) {
  (state + msg, state + msg)
}

fn test_two_actors() -> (Int, Int) {
  let a := conc.spawn(0, counter)
  let b := conc.spawn(100, counter)
  let ra := conc.ask(a, 7)
  let rb := conc.ask(b, 7)
  (ra, rb)
}
"#;
    assert_eq!(
        run(src, "test_two_actors", vec![]),
        Value::Tuple(vec![Value::Int(7), Value::Int(107)])
    );
}
