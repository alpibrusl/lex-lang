//! Conformance for `list.par_map` (#305 slice 1).
//!
//! Asserts:
//! - API shape: signature parses, type-checks, executes, returns
//!   results in input order.
//! - Wall-clock parallelism: N pure CPU-bound closures complete in
//!   measurably less than N × per-task time when the host has
//!   ≥2 available cores. The test self-skips on single-core hosts
//!   (a thread pool with one slot can't show parallelism).
//! - `LEX_PAR_MAX_CONCURRENCY` caps the pool size: setting it to 1
//!   forces serial execution and brings wall-clock back to N ×
//!   per-task time.
//! - Effectful closures fail with a clear error (slice 1 limitation).

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, DefaultHandler, Policy};
use lex_syntax::parse_source;

fn build(src: &str) -> lex_bytecode::Program {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    check_program(&bc, &Policy::pure()).expect("program type-checks under pure policy");
    bc
}

fn run(bc: &lex_bytecode::Program, entry: &str, args: Vec<Value>) -> Value {
    let handler = DefaultHandler::new(Policy::pure());
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    vm.call(entry, args).unwrap()
}

#[test]
fn par_map_returns_results_in_input_order() {
    // A pure closure that doubles its input — slice 1 forbids
    // effects in par_map's body, so doubling is the simplest
    // round-trip.
    let src = r#"
import "std.list" as list
fn doubled(xs :: List[Int]) -> List[Int] {
    list.par_map(xs, fn(x :: Int) -> Int { x + x })
}
"#;
    let bc = build(src);
    let xs: Vec<Value> = (0..8).map(Value::Int).collect();
    let r = run(&bc, "doubled", vec![Value::List(xs)]);
    let expected: Vec<Value> = (0..8).map(|i: i64| Value::Int(i * 2)).collect();
    assert_eq!(r, Value::List(expected));
}

#[test]
fn par_map_on_empty_list_yields_empty_list() {
    let src = r#"
import "std.list" as list
fn run_(xs :: List[Int]) -> List[Int] {
    list.par_map(xs, fn(x :: Int) -> Int { x })
}
"#;
    let bc = build(src);
    let r = run(&bc, "run_", vec![Value::List(vec![])]);
    assert_eq!(r, Value::List(vec![]));
}

/// Pure CPU spin: count list elements (via `list.fold`, which is
/// inline-emitted as a bytecode loop and therefore handler-free).
/// The caller passes a pre-built list whose length controls the
/// per-task duration. Tail-recursion or `list.range` would dispatch
/// through the handler (slice-1 worker is `DenyAllEffects`), so we
/// avoid both.
const SPIN_SRC: &str = r#"
import "std.list" as list
fn spin(xs :: List[Int]) -> Int {
    list.fold(xs, 0, fn(acc :: Int, x :: Int) -> Int { acc + 1 })
}
fn par_spins(buckets :: List[List[Int]]) -> List[Int] {
    list.par_map(buckets, fn(b :: List[Int]) -> Int { spin(b) })
}
"#;

fn measure_par_spin(n_workers: usize, items_per_bucket: usize) -> std::time::Duration {
    let bc = build(SPIN_SRC);
    let bucket: Vec<Value> = (0..items_per_bucket as i64).map(Value::Int).collect();
    let buckets: Vec<Value> = (0..n_workers).map(|_| Value::List(bucket.clone())).collect();
    let t0 = std::time::Instant::now();
    let _ = run(&bc, "par_spins", vec![Value::List(buckets)]);
    t0.elapsed()
}

// Wall-clock parallelism + the `LEX_PAR_MAX_CONCURRENCY` cap (#305
// slice 1 AC). Combined because `std::env::set_var` is process-
// global; two parallel-running tests that toggle the same var
// would race.
//
// Marked `#[ignore]` because sandboxed CI runners frequently only
// give one wall-clock CPU even when `available_parallelism()`
// reports more — a baseline `cargo test` would flake on the
// 70%-of-serial assertion below. Run locally or under a real
// multi-core CI as:
//     cargo test --test list_par_map -- --ignored --test-threads=1
#[test]
#[ignore]
fn par_map_speedup_and_concurrency_cap() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 2 {
        eprintln!("skipping: single-core host can't demonstrate parallelism");
        return;
    }
    // 8k items per task → ~50-100ms per task on a typical CI core,
    // small enough that 4 sequential tasks finish in ~300-400ms.
    const ITEMS_PER_BUCKET: usize = 8_000;
    let n_tasks = cores.min(4);

    // Make sure no stale cap is hanging around from a sibling test.
    std::env::remove_var("LEX_PAR_MAX_CONCURRENCY");

    // Baseline: one task at the default cap.
    let one = measure_par_spin(1, ITEMS_PER_BUCKET);
    // N tasks at the default cap → real parallelism.
    let parallel = measure_par_spin(n_tasks, ITEMS_PER_BUCKET);
    // N tasks forced to serial via the cap.
    std::env::set_var("LEX_PAR_MAX_CONCURRENCY", "1");
    let capped = measure_par_spin(n_tasks, ITEMS_PER_BUCKET);
    std::env::remove_var("LEX_PAR_MAX_CONCURRENCY");

    // Parallel run should beat 70% of the serial-equivalent ceiling.
    let serial_equiv = one * (n_tasks as u32);
    let ceiling = serial_equiv.mul_f64(0.70);
    assert!(
        parallel < ceiling,
        "par_map should beat 70% of serial wall-clock: one={one:?}, \
         parallel({n_tasks} tasks)={parallel:?}, ceiling={ceiling:?}"
    );
    // Capped run should be measurably slower than the parallel run.
    // 1.4× is conservative for noisy CI; in practice we see 2-3×.
    assert!(
        capped > parallel.mul_f64(1.4),
        "cap=1 must dominate parallel run: parallel={parallel:?}, capped={capped:?}"
    );
}

#[test]
fn par_map_results_are_correct_under_concurrency_cap_of_one() {
    // Even when `LEX_PAR_MAX_CONCURRENCY=1` forces a single worker
    // thread, par_map must still produce the right results in input
    // order. This is the sandbox-friendly counterpart to the
    // `#[ignore]`-d wall-clock test: it exercises the cap path
    // without depending on the runner having real parallelism.
    std::env::set_var("LEX_PAR_MAX_CONCURRENCY", "1");
    let src = r#"
import "std.list" as list
fn squared(xs :: List[Int]) -> List[Int] {
    list.par_map(xs, fn(x :: Int) -> Int { x * x })
}
"#;
    let bc = build(src);
    let xs: Vec<Value> = (0..16).map(Value::Int).collect();
    let r = run(&bc, "squared", vec![Value::List(xs)]);
    std::env::remove_var("LEX_PAR_MAX_CONCURRENCY");
    let expected: Vec<Value> = (0..16).map(|i: i64| Value::Int(i * i)).collect();
    assert_eq!(r, Value::List(expected));
}

#[test]
fn par_map_distributes_when_n_exceeds_cap() {
    // 32 items but cap=4 forces the runtime to multiplex multiple
    // items per worker. Results must still come back in input order.
    std::env::set_var("LEX_PAR_MAX_CONCURRENCY", "4");
    let src = r#"
import "std.list" as list
fn run_(xs :: List[Int]) -> List[Int] {
    list.par_map(xs, fn(x :: Int) -> Int { x + 1000 })
}
"#;
    let bc = build(src);
    let xs: Vec<Value> = (0..32).map(Value::Int).collect();
    let r = run(&bc, "run_", vec![Value::List(xs)]);
    std::env::remove_var("LEX_PAR_MAX_CONCURRENCY");
    let expected: Vec<Value> = (0..32).map(|i: i64| Value::Int(i + 1000)).collect();
    assert_eq!(r, Value::List(expected));
}

#[test]
fn par_map_effectful_closure_is_refused() {
    // Slice 1: effectful closures get DenyAllEffects in the worker.
    // The closure compiles and type-checks; it fails at runtime when
    // it attempts to dispatch an effect.
    let src = r#"
import "std.list" as list
import "std.io" as io
fn echo_par(xs :: List[Str]) -> [io] List[Unit] {
    list.par_map(xs, fn(s :: Str) -> [io] Unit { io.print(s) })
}
"#;
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    let mut policy = Policy::pure();
    policy.allow_effects.insert("io".into());
    check_program(&bc, &policy).expect("type-checks under io policy");
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let err = vm
        .call("echo_par", vec![Value::List(vec![Value::Str("hi".into())])])
        .expect_err("slice 1 must reject effectful par_map closures");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("effect") || msg.contains("Effect"),
        "expected an effect-refusal error, got: {msg}"
    );
}
