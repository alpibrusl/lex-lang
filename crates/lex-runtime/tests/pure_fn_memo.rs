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

/// Like `run_with_counters` but also surfaces the adaptive-memo skip
/// counter (#229 adaptive) — the number of effect-free calls that
/// bypassed the cache because their function was disabled.
fn run_with_skips(src: &str, fn_name: &str, args: Vec<Value>)
    -> (Value, u64, u64, u64)
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
    (v, vm.pure_memo_hits, vm.pure_memo_misses, vm.pure_memo_skips)
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

// ---------------------------------------------------------------------------
// Adaptive memoization (#229 adaptive): a pure function whose args never
// repeat pays the args-hash on every call for zero benefit. After a warmup
// window (MEMO_WARMUP_CALLS = 64) with zero hits, the VM disables
// memoization for that function and its subsequent calls skip the hash.
// Disabling is always safe — the callee is pure, so the plain path
// recomputes the identical value.
// ---------------------------------------------------------------------------

// `leaf` is called once per `drive` recursion level, each time with a
// distinct argument, so it never hits its own cache. `drive` likewise
// recurses on a distinct `n` each level. With > 64 levels both functions
// blow through the warmup window and get disabled, after which their calls
// register as skips rather than misses.
const ALWAYS_MISS_SRC: &str = r#"
fn leaf(n :: Int) -> Int { n + 1 }

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => leaf(n) + drive(n - 1),
  }
}
"#;

#[test]
fn cold_function_is_disabled_after_warmup_and_skips() {
    // drive(200): 200 distinct-arg calls to `leaf` and ~200 self-calls to
    // `drive`. Both exceed the 64-call warmup with zero hits, so both get
    // disabled and the post-warmup calls show up as skips.
    let (v, hits, _misses, skips) = run_with_skips(ALWAYS_MISS_SRC, "drive", vec![Value::Int(200)]);
    // sum_{k=1..200} (k+1) = sum k + 200 = 20100 + 200 = 20300
    assert_eq!(v, Value::Int(20300), "result must be correct despite disabling");
    assert_eq!(hits, 0, "always-distinct args never hit");
    assert!(skips > 0, "expected adaptive memo to disable a cold function and skip; got 0 skips");
}

#[test]
fn cold_function_result_matches_non_adaptive_recompute() {
    // Correctness guard: the value must be identical whether memoization
    // stayed on the whole time (n <= warmup, never disabled) or got
    // disabled partway (n > warmup). Compute both and compare.
    let (small, _, _, skips_small) =
        run_with_skips(ALWAYS_MISS_SRC, "drive", vec![Value::Int(10)]);
    let (large, _, _, skips_large) =
        run_with_skips(ALWAYS_MISS_SRC, "drive", vec![Value::Int(100)]);
    // n=10 stays under warmup → no disabling; n=100 trips it.
    assert_eq!(skips_small, 0, "under warmup, nothing should be disabled");
    assert!(skips_large > 0, "over warmup, cold functions should disable");
    // Closed-form: drive(n) = sum_{k=1..n}(k+1) = n(n+1)/2 + n.
    assert_eq!(small, Value::Int(10 * 11 / 2 + 10));   // 65
    assert_eq!(large, Value::Int(100 * 101 / 2 + 100)); // 5150
}

// Naive recursive fib reuses sub-results heavily, so it hits its cache
// almost immediately and should NEVER be disabled — memoization is exactly
// what makes it fast. Verify it accumulates hits and records zero skips.
const FIB_SRC: &str = r#"
fn fib(n :: Int) -> Int {
  match n {
    0 => 0,
    1 => 1,
    _ => fib(n - 1) + fib(n - 2),
  }
}
"#;

#[test]
fn hot_function_stays_enabled_no_skips() {
    let (v, hits, _misses, skips) = run_with_skips(FIB_SRC, "fib", vec![Value::Int(25)]);
    assert_eq!(v, Value::Int(75025), "fib(25)");
    assert!(hits > 0, "naive fib must hit its memo cache");
    assert_eq!(skips, 0, "a function that hits should never be disabled");
}
