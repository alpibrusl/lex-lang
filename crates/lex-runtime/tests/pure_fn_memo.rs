//! Acceptance tests for #229: pure-function memoization in the VM.
//!
//! - Same input twice → second call hits the cache, doesn't re-execute.
//! - Different inputs to the same fn → cached independently.
//! - Effectful function (any declared effect) → never cached.
//! - Cache is per-VM: two separate `Vm::with_handler` invocations
//!   start with empty caches.
//! - The hit/miss counters surface for observability.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run_with_counters(src: &str, fn_name: &str, args: Vec<Value>)
    -> (Value, u64, u64)
{
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::permissive())
        .with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let v = vm.call(fn_name, args).unwrap_or_else(|e| panic!("{e:?}"));
    (v, vm.pure_memo_hits, vm.pure_memo_misses)
}

const PURE_SRC: &str = r#"
# A pure function (no declared effects). Calling it with the same
# arg twice should hit the cache the second time.
fn double(n :: Int) -> Int { n + n }

fn calls_twice_same() -> (Int, Int) {
  (double(7), double(7))
}

fn calls_twice_different() -> (Int, Int) {
  (double(7), double(9))
}

# Three calls with two distinct inputs: 7 (twice) and 9 (once).
# Expect 1 hit (the second double(7)) and 2 misses (first double(7),
# only call to double(9)).
fn three_calls() -> Int {
  let _a := double(7)
  let _b := double(9)
  let _c := double(7)
  0
}
"#;

#[test]
fn same_input_twice_hits_cache_on_second_call() {
    let (_, hits, misses) = run_with_counters(PURE_SRC, "calls_twice_same", vec![]);
    assert_eq!(hits, 1, "second double(7) should hit cache; got {hits} hits");
    assert_eq!(misses, 1, "first double(7) is the only miss; got {misses} misses");
}

#[test]
fn different_inputs_are_cached_independently() {
    let (_, hits, misses) = run_with_counters(PURE_SRC, "calls_twice_different", vec![]);
    assert_eq!(hits, 0, "two different inputs → both miss");
    assert_eq!(misses, 2);
}

#[test]
fn three_calls_two_distinct_inputs_have_one_hit_two_misses() {
    let (_, hits, misses) = run_with_counters(PURE_SRC, "three_calls", vec![]);
    assert_eq!(hits, 1);
    assert_eq!(misses, 2);
}

const EFFECTFUL_SRC: &str = r#"
import "std.io" as io
# This function declares [io] (a real effect). The memoization
# pass must skip it: caching IO output would change observable
# behavior on the second call.
fn print_n(n :: Int) -> [io] Int {
  io.print("called")
  n + 1
}

fn calls_twice() -> [io] Int {
  let _a := print_n(5)
  let _b := print_n(5)
  0
}
"#;

#[test]
fn effectful_function_is_never_memoized() {
    let (_, hits, misses) = run_with_counters(EFFECTFUL_SRC, "calls_twice", vec![]);
    assert_eq!(hits, 0,
        "function with [io] must never enter the memoization cache");
    assert_eq!(misses, 0,
        "function with [io] must never even attempt the cache");
}

const RECORD_ARG_SRC: &str = r#"
# Confirm that Record arguments hash deterministically — two
# semantically-equal Records produced from the same field values
# should hit the same cache entry.
fn extract(r :: { name :: Str, qty :: Int }) -> Int { r.qty }

fn calls_with_same_record_twice() -> (Int, Int) {
  ( extract({ name: "x", qty: 1 })
  , extract({ name: "x", qty: 1 })
  )
}
"#;

#[test]
fn record_argument_hashes_deterministically() {
    let (_, hits, misses) = run_with_counters(
        RECORD_ARG_SRC, "calls_with_same_record_twice", vec![]);
    assert_eq!(hits, 1);
    assert_eq!(misses, 1);
}

const CROSS_VM_SRC: &str = r#"
fn double(n :: Int) -> Int { n + n }
# Non-tail position so this goes through Op::Call (which is the
# v1 memoization site). A tail-position call would emit
# Op::TailCall and bypass the cache by design — see the comment
# at Op::CallClosure in the VM (#229).
fn one_call() -> Int {
  let r := double(7)
  r
}
"#;

#[test]
fn cache_does_not_persist_across_vm_invocations() {
    // Two separate Vm::with_handler calls each compute one miss;
    // neither sees the other's cache. This is the per-run scope
    // the issue prescribes.
    let (_, h1, m1) = run_with_counters(CROSS_VM_SRC, "one_call", vec![]);
    let (_, h2, m2) = run_with_counters(CROSS_VM_SRC, "one_call", vec![]);
    assert_eq!(h1, 0); assert_eq!(m1, 1);
    assert_eq!(h2, 0); assert_eq!(m2, 1);
}
