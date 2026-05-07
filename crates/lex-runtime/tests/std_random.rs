//! Integration tests for `std.random` (#219).
//!
//! Pins the design points that make this RNG worth its keep:
//!   - **Byte-identical sequences across runs.** Same seed →
//!     same draws, no exceptions.
//!   - **Pure / value-threaded.** No global state; the caller
//!     forwards the `Rng` value across calls.
//!   - **Replay determinism.** A trace that records the seed at
//!     construction time can replay all subsequent draws exactly.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.random" as rng
import "std.tuple"  as tup

# Three int draws threaded through one Rng.
fn three_ints(seed :: Int) -> (Int, Int, Int) {
  let r0 := rng.seed(seed)
  let s1 := rng.int(r0, 0, 1000000)
  let s2 := rng.int(tup.snd(s1), 0, 1000000)
  let s3 := rng.int(tup.snd(s2), 0, 1000000)
  (tup.fst(s1), tup.fst(s2), tup.fst(s3))
}

# Float draw — confined to [0.0, 1.0).
fn one_float(seed :: Int) -> Float {
  tup.fst(rng.float(rng.seed(seed)))
}

# Range respect — every draw should land in [lo, hi].
fn ranged_int(seed :: Int, lo :: Int, hi :: Int) -> Int {
  tup.fst(rng.int(rng.seed(seed), lo, hi))
}

# choose on a non-empty list returns Some(elem); on empty, None.
fn pick_from(seed :: Int, xs :: List[Int]) -> Option[Int] {
  match rng.choose(rng.seed(seed), xs) {
    Some(picked) => Some(tup.fst(picked)),
    None => None,
  }
}
"#;

fn compile_and_handler() -> (Arc<lex_bytecode::Program>, DefaultHandler) {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    (bc, handler)
}

fn call(name: &str, args: Vec<Value>) -> Value {
    let (bc, handler) = compile_and_handler();
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(name, args).unwrap_or_else(|e| panic!("call {name}: {e}"))
}

#[test]
fn same_seed_produces_same_three_ints() {
    let a = call("three_ints", vec![Value::Int(42)]);
    let b = call("three_ints", vec![Value::Int(42)]);
    assert_eq!(a, b);
    // Sanity: three draws shouldn't all be equal — that would
    // mean the Rng isn't advancing between calls.
    if let Value::Tuple(xs) = &a {
        assert!(xs[0] != xs[1] || xs[1] != xs[2],
                "all three draws equal — Rng not advancing? got {xs:?}");
    } else {
        panic!("expected Tuple, got {a:?}");
    }
}

#[test]
fn different_seeds_produce_different_sequences() {
    let a = call("three_ints", vec![Value::Int(1)]);
    let b = call("three_ints", vec![Value::Int(2)]);
    assert_ne!(a, b, "seed=1 and seed=2 produced the same sequence");
}

#[test]
fn float_draw_lands_in_unit_interval() {
    for seed in [0i64, 1, 7, 42, -1, i64::MAX] {
        let v = call("one_float", vec![Value::Int(seed)]);
        let f = match v {
            Value::Float(f) => f,
            other => panic!("expected Float, got {other:?}"),
        };
        assert!((0.0..1.0).contains(&f),
                "seed={seed} produced f={f}, not in [0.0, 1.0)");
    }
}

#[test]
fn ranged_int_respects_bounds() {
    // Sample the same seed across a few ranges; verify every
    // draw is bounded. Doesn't verify uniformity (that's covered
    // by inspection of the SplitMix64 output, not behavior).
    for seed in 0..20i64 {
        let v = call(
            "ranged_int",
            vec![Value::Int(seed), Value::Int(10), Value::Int(20)],
        );
        match v {
            Value::Int(n) => assert!((10..=20).contains(&n),
                "seed={seed} drew {n}, outside [10, 20]"),
            other => panic!("expected Int, got {other:?}"),
        }
    }
}

#[test]
fn ranged_int_collapses_to_singleton_when_lo_eq_hi() {
    let v = call(
        "ranged_int",
        vec![Value::Int(99), Value::Int(7), Value::Int(7)],
    );
    assert_eq!(v, Value::Int(7));
}

#[test]
fn choose_returns_some_for_nonempty_list() {
    let xs = Value::List(vec![Value::Int(10), Value::Int(20), Value::Int(30)]);
    let v = call("pick_from", vec![Value::Int(123), xs]);
    match v {
        Value::Variant { name, args } => {
            assert_eq!(name, "Some");
            match args.first() {
                Some(Value::Int(n)) => assert!([10, 20, 30].contains(n)),
                other => panic!("expected Int payload, got {other:?}"),
            }
        }
        other => panic!("expected Variant, got {other:?}"),
    }
}

#[test]
fn choose_returns_none_for_empty_list() {
    let v = call("pick_from", vec![Value::Int(7), Value::List(vec![])]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "None"),
        other => panic!("expected Variant, got {other:?}"),
    }
}
