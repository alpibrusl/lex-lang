//! #464 tuple codegen — acceptance test for a tuple-heavy workload.
//!
//! The companion harness (`examples/profile_tuple_build.rs`) reports
//! the callgrind instruction delta for humans; this test is the
//! CI-runnable regression gate:
//!
//!  1. **Every non-escaping tuple lowers and runs on the stack path**
//!     — exact, measured via static op-site counts plus the per-VM
//!     `Vm::stack_record_allocs` / `stack_record_heap_fallbacks`
//!     counters (`AllocStackTuple`'s stack path shares the
//!     record arena counter). Fully deterministic.
//!
//!  2. **Speedup with lowering enabled** — wall-clock A/B over a tight
//!     loop, `#[ignore]`d by default (timing is noisy on shared CI).
//!     Run explicitly for the number; the assertion uses a relaxed
//!     floor as a regression gate only.

use std::sync::Arc;
use std::time::Instant;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Op, Program, Value};
use lex_syntax::parse_source;

/// Six non-escaping intermediate tuples per `handle` call, each built
/// and destructured via `match` (the surface construct that emits
/// `MakeTuple` + `GetElem`). `drive` passes scalars and reads the
/// scalar result, so no tuple escapes across the call boundary.
const SRC: &str = r#"
fn handle(a :: Int, b :: Int) -> Int {
  let s1 := match (a, b)   { (x, y) => x + y }
  let s2 := match (a, b)   { (x, y) => x * y }
  let s3 := match (s1, s2) { (x, y) => x + y }
  let s4 := match (s1, s2) { (x, y) => x - y }
  let s5 := match (s3, s4) { (x, y) => x * 2 + y }
  let s6 := match (s4, s3) { (x, y) => x + y * 3 }
  s1 + s2 + s3 + s4 + s5 + s6
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n, 7)
      r + drive(n - 1)
    },
  }
}
"#;

fn compile_with_env(src: &str, no_stack: bool) -> Arc<Program> {
    // The `unsafe` is required by Rust 2024's audited env API. This
    // test is single-threaded: set the flag, compile, unset.
    // Slice 2b-i note: the disabled arm now also suppresses arena
    // lowering, so the A/B is a true "no record/tuple lowering" vs
    // "all lowering" rather than partially-arena-only.
    if no_stack {
        unsafe { std::env::set_var("LEX_NO_STACK_RECORDS", "1"); }
        unsafe { std::env::set_var("LEX_NO_ARENA_RECORDS", "1"); }
    }
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));
    if no_stack {
        unsafe { std::env::remove_var("LEX_NO_STACK_RECORDS"); }
        unsafe { std::env::remove_var("LEX_NO_ARENA_RECORDS"); }
    }
    p
}

fn count_tuple_sites(p: &Program) -> (usize, usize) {
    let mut make_tuple = 0usize;
    let mut alloc_stack = 0usize;
    for f in &p.functions {
        for op in &f.code {
            match op {
                Op::MakeTuple(_) => make_tuple += 1,
                Op::AllocStackTuple { .. } => alloc_stack += 1,
                _ => {}
            }
        }
    }
    (make_tuple, alloc_stack)
}

/// Bar 1: every non-escaping tuple lowers to `AllocStackTuple` and
/// runs on the stack path. Exact, counter-driven, no timing noise.
#[test]
fn all_non_escaping_tuples_lower_and_run_on_stack() {
    let p = compile_with_env(SRC, false);
    let (make_tuple, alloc_stack) = count_tuple_sites(&p);
    // The 6 `handle` tuples are non-escaping; `drive`'s match is on an
    // Int (no tuple). All 6 lower; none stay on the heap.
    assert_eq!(alloc_stack, 6, "expected all 6 tuple sites to lower");
    assert_eq!(make_tuple, 0, "no heap MakeTuple should remain after lowering");

    // The disabled arm must keep them all on the heap — proves the
    // A/B baseline the bench/profile harness relies on is real.
    let disabled = compile_with_env(SRC, true);
    let (make_tuple_off, alloc_stack_off) = count_tuple_sites(&disabled);
    assert_eq!(alloc_stack_off, 0, "lowering should be fully suppressed");
    assert_eq!(make_tuple_off, 6, "all 6 tuples should stay MakeTuple");

    // Runtime: drive(200) calls handle 200 times, 6 stack tuples each.
    // Each handle frame uses 6×2 = 12 arena slots ≪ the 64-slot
    // budget, so nothing falls back.
    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    let r = vm.call("drive", vec![Value::Int(200)]).unwrap();
    assert!(matches!(r, Value::Int(_)), "expected Int return, got {r:?}");

    println!(
        "[#464 tuple] stack={}, fallback={}",
        vm.stack_record_allocs, vm.stack_record_heap_fallbacks
    );
    assert_eq!(vm.stack_record_allocs, 200 * 6,
        "expected 6 stack tuples per handle × 200 calls");
    assert_eq!(vm.stack_record_heap_fallbacks, 0,
        "no budget exhaustion expected for this workload");

    // Both arms must compute the same answer.
    let mut vm_off = Vm::new(&disabled);
    vm_off.set_step_limit(u64::MAX);
    let r_off = vm_off.call("drive", vec![Value::Int(200)]).unwrap();
    assert_eq!(r, r_off, "stack and heap paths must agree");
}

/// Bar 2: speedup with lowering. Timing-based → `#[ignore]`d; relaxed
/// floor as a regression gate. Run with `--ignored --nocapture` for
/// the number (the callgrind harness gives the deterministic count).
#[test]
#[ignore = "timing-based; see doc comment"]
fn tuple_lowering_speedup() {
    let enabled = compile_with_env(SRC, false);
    let disabled = compile_with_env(SRC, true);

    for prog in [&enabled, &disabled] {
        let mut vm = Vm::new(prog);
        vm.set_step_limit(u64::MAX);
        let _ = vm.call("drive", vec![Value::Int(50)]).unwrap();
    }

    let n: i64 = 500;
    let iters: usize = 200;
    let time_arm = |prog: &Arc<Program>| -> f64 {
        let t0 = Instant::now();
        for _ in 0..iters {
            let mut vm = Vm::new(prog);
            vm.set_step_limit(u64::MAX);
            std::hint::black_box(vm.call("drive", vec![Value::Int(n)]).unwrap());
        }
        t0.elapsed().as_secs_f64()
    };

    let mut on = [time_arm(&enabled), time_arm(&enabled), time_arm(&enabled)];
    let mut off = [time_arm(&disabled), time_arm(&disabled), time_arm(&disabled)];
    on.sort_by(|a, b| a.partial_cmp(b).unwrap());
    off.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ratio = off[0] / on[0];
    println!("[#464 tuple] best-of-3: enabled={:.4}s disabled={:.4}s speedup={ratio:.2}×",
        on[0], off[0]);
    assert!(ratio >= 1.15,
        "speedup {ratio:.2}× below the 1.15× regression floor");
}
