//! #1085: `conc.ask_async` / `conc.await` — actor messages that run on
//! their own OS thread, so asks to different actors proceed in
//! parallel, effects included, while each actor still processes its
//! messages one at a time in send order.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

fn compile(src: &str) -> lex_bytecode::Program {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    lex_bytecode::compile_program(&stages)
}

fn run(src: &str, entry: &str) -> Result<Value, lex_bytecode::vm::VmError> {
    let bc = compile(src);
    let policy = Policy::permissive();
    check_program(&bc, &policy).expect("policy check");
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(entry, vec![])
}

fn ints(xs: &[i64]) -> Value {
    Value::List(VecDeque::from(xs.iter().map(|&x| Value::Int(x)).collect::<Vec<_>>()).into())
}

#[test]
fn ask_async_then_await_returns_reply() {
    let src = r#"
import "std.conc" as conc

fn counter(state :: Int, msg :: Int) -> (Int, Int) {
  (state + msg, state + msg)
}

fn go() -> [concurrent] Int {
  let a := conc.spawn(10, counter)
  let h :: AskHandle[Int] := conc.ask_async(a, 5)
  conc.await(h)
}
"#;
    assert_eq!(run(src, "go").unwrap(), Value::Int(15));
}

#[test]
fn messages_stay_fifo_across_async_and_sync_sends() {
    // Every message appends to the state; the final list is the order
    // the actor actually processed them in, which must be send order
    // even though the async ones run on worker threads.
    let src = r#"
import "std.conc" as conc
import "std.list" as list

fn log(state :: List[Int], msg :: Int) -> (List[Int], List[Int]) {
  let next := list.concat(state, [msg])
  (next, next)
}

fn go() -> [concurrent] List[Int] {
  let a := conc.spawn([], log)
  let h1 := conc.ask_async(a, 1)
  let h2 := conc.ask_async(a, 2)
  let _ := conc.tell(a, 3)
  let h4 := conc.ask_async(a, 4)
  let _ := conc.await(h2)
  let _ := conc.await(h1)
  let _ := conc.await(h4)
  conc.ask(a, 5)
}
"#;
    assert_eq!(run(src, "go").unwrap(), ints(&[1, 2, 3, 4, 5]));
}

#[test]
fn different_actors_run_effectful_handlers_in_parallel() {
    // Four actors each sleep 400ms per message. Sequentially that is
    // ≥ 1.6s; fanned out with ask_async it should take ~one sleep. The
    // handler performs a real `time` effect, so this also proves the
    // worker thread has a working effect handler (not DenyAllEffects).
    let src = r#"
import "std.conc" as conc
import "std.time" as time

fn slow(state :: Int, msg :: Int) -> [time] (Int, Int) {
  let _ := time.sleep_ms(400)
  (state + msg, state + msg)
}

fn go() -> [concurrent] List[Int] {
  let a := conc.spawn(0, slow)
  let b := conc.spawn(100, slow)
  let c := conc.spawn(200, slow)
  let d := conc.spawn(300, slow)
  let ha := conc.ask_async(a, 1)
  let hb := conc.ask_async(b, 1)
  let hc := conc.ask_async(c, 1)
  let hd := conc.ask_async(d, 1)
  [conc.await(ha), conc.await(hb), conc.await(hc), conc.await(hd)]
}
"#;
    let start = Instant::now();
    let out = run(src, "go").unwrap();
    let elapsed = start.elapsed();
    assert_eq!(out, ints(&[1, 101, 201, 301]));
    assert!(
        elapsed < Duration::from_millis(1200),
        "expected parallel execution (~400ms), took {elapsed:?}"
    );
}

#[test]
fn handler_error_surfaces_at_await_and_actor_keeps_working() {
    let src = r#"
import "std.conc" as conc

fn divider(state :: Int, msg :: Int) -> (Int, Int) {
  (state, 100 / msg)
}

fn bad() -> [concurrent] Int {
  let a := conc.spawn(0, divider)
  let h := conc.ask_async(a, 0)
  conc.await(h)
}

fn recovers() -> [concurrent] Int {
  let a := conc.spawn(0, divider)
  let _h := conc.ask_async(a, 0)
  conc.ask(a, 4)
}
"#;
    let err = run(src, "bad").unwrap_err();
    assert!(format!("{err:?}").contains("conc.await"), "got {err:?}");
    // The failed message still releases its turn, so the next message
    // runs instead of waiting forever.
    assert_eq!(run(src, "recovers").unwrap(), Value::Int(25));
}

#[test]
fn falls_back_to_inline_without_a_worker_handler() {
    // `Vm::new` uses `DenyAllEffects`, which offers no per-thread
    // handler; ask_async must still deliver (inline) rather than fail.
    let src = r#"
import "std.conc" as conc

fn counter(state :: Int, msg :: Int) -> (Int, Int) {
  (state + msg, state + msg)
}

fn go() -> [concurrent] Int {
  let a := conc.spawn(1, counter)
  let h := conc.ask_async(a, 2)
  let _ := conc.tell(a, 3)
  conc.await(h) + conc.ask(a, 0)
}
"#;
    let bc = compile(src);
    let mut vm = Vm::new(&bc);
    // conc.* is intercepted by the VM itself, so no effect handler is needed.
    assert_eq!(vm.call("go", vec![]).unwrap(), Value::Int(3 + 6));
}
