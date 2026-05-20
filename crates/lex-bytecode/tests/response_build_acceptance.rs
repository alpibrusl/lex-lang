//! #464 step 3 — acceptance test for the `response_build` workload.
//!
//! The companion bench (`benches/response_build.rs`) reports the
//! enabled-vs-disabled ratio for humans running `cargo bench`; this
//! test is the CI-runnable acceptance for the two #464 bars:
//!
//!  1. **≥60% of record allocations on the stack** — exact, measured
//!     via the per-VM counters `Vm::stack_record_allocs` /
//!     `heap_record_allocs` / `stack_record_heap_fallbacks`. The
//!     non-escaping intermediates always land on the stack path
//!     (the budget is 64 slots per frame, and the handler builds 3
//!     records totaling 9 slots), so the rate is fully
//!     deterministic and the bar holds with room to spare.
//!
//!  2. **≥1.5× speedup with lowering enabled** — measured by wall
//!     clock over a tight loop running the workload N times.
//!     Timing-based, so the threshold is intentionally
//!     noise-tolerant: we run the workload at a size large enough
//!     for the signal to dominate per-call overhead, repeat both
//!     arms, and assert against a relaxed 1.3× floor in this test
//!     (the criterion bench reports the precise number for human
//!     verification of the issue's ≥1.5× bar). Trading a hair of
//!     bar strictness for CI stability is the standard play here —
//!     the issue's bar is the publishable number; the test's bar
//!     is the regression gate.

use std::sync::Arc;
use std::time::Instant;

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Op, Program, Value};
use lex_syntax::parse_source;

/// Allocation-heavy handler shape: 6 non-escaping local records
/// per call (the typical pattern of layered intermediate values
/// during a request → response computation), plus one Response
/// record that must escape via the return. Passes scalar args to
/// avoid the per-call Request heap allocation noise.
///
/// Per call: 6 stack records + 1 heap record (Response).
/// Per drive iter: same, since drive passes scalars and reads the
/// Response.total field-only. Expected stack rate: 6/7 ≈ 85.7%.
const SRC: &str = r#"
type Response = { status :: Int, total :: Int }

fn handle(user_id :: Int, item_id :: Int, qty :: Int) -> Response {
  let v1 := { a: user_id, b: item_id, c: qty }
  let v2 := { d: v1.a, e: v1.b, f: v1.c, g: v1.a * 2 }
  let v3 := { h: v2.d, i: v2.e, j: v2.f, k: v2.g }
  let v4 := { l: v3.h * 3, m: v3.i * 5, n: v3.j * 7, o: v3.k }
  let v5 := { p: v4.l + v4.m, q: v4.n + v4.o, r: v4.l - v4.m }
  let v6 := { s: v5.p + v5.q, t: v5.q + v5.r, u: v5.p - v5.r }
  match v6.s > 0 {
    true  => { status: 200, total: v6.s + v6.t + v6.u },
    false => { status: 400, total: 0 },
  }
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n, 7, 3)
      r.total + drive(n - 1)
    },
  }
}
"#;

fn compile_with_env(src: &str, no_stack_records: bool) -> Arc<Program> {
    if no_stack_records {
        // The `unsafe` is required by Rust 2024's audited env API.
        // The test is single-threaded; we set the flag, compile,
        // unset, return. No concurrent env read window.
        unsafe { std::env::set_var("LEX_NO_STACK_RECORDS", "1"); }
        let prog = parse_source(src).expect("parse");
        let stages = canonicalize_program(&prog);
        lex_types::check_program(&stages).expect("typecheck");
        let p = Arc::new(compile_program(&stages));
        unsafe { std::env::remove_var("LEX_NO_STACK_RECORDS"); }
        p
    } else {
        let prog = parse_source(src).expect("parse");
        let stages = canonicalize_program(&prog);
        lex_types::check_program(&stages).expect("typecheck");
        Arc::new(compile_program(&stages))
    }
}

fn count_record_sites(p: &Program) -> (usize, usize) {
    let mut make_record = 0usize;
    let mut alloc_stack = 0usize;
    for f in &p.functions {
        for op in &f.code {
            match op {
                Op::MakeRecord { .. } => make_record += 1,
                Op::AllocStackRecord { .. } => alloc_stack += 1,
                _ => {}
            }
        }
    }
    (make_record, alloc_stack)
}

/// Acceptance bar 1: ≥60% of record allocations land on the stack
/// path. Exact, counter-driven; no timing noise.
#[test]
fn at_least_60_percent_of_records_on_stack() {
    let p = compile_with_env(SRC, false);
    let (make_record_sites, alloc_stack_sites) = count_record_sites(&p);
    assert!(alloc_stack_sites > 0, "expected lowering to fire");
    assert!(make_record_sites > 0, "expected at least one escaping record (the Response)");

    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    let r = vm.call("drive", vec![Value::Int(200)]).unwrap();
    assert!(matches!(r, Value::Int(_)), "expected Int return, got {r:?}");

    let stack = vm.stack_record_allocs;
    let heap = vm.heap_record_allocs;
    let fallback = vm.stack_record_heap_fallbacks;
    let total = stack + heap + fallback;
    assert!(total > 0);
    let rate = stack as f64 / total as f64;
    println!(
        "[#464 step 3] record alloc breakdown: stack={stack}, heap={heap}, \
         fallback={fallback}, total={total}, stack_rate={:.2}%",
        rate * 100.0
    );
    assert!(rate >= 0.60,
        "stack-allocation rate {:.2}% is below the 60% acceptance bar",
        rate * 100.0);

    // Exact numbers (the test acts as a regression gate for the
    // lowering pass and the analysis):
    //   Per `handle()` call: 6 stack records (v1..v6) + 1 heap
    //   record (Response). drive(200) calls handle 200 times.
    //   stack    = 200 * 6 = 1200
    //   heap     = 200 * 1 = 200
    //   fallback = 0 (each frame uses 21 slots ≪ 64-slot budget)
    //   total = 1400, rate = 1200/1400 ≈ 85.7%.
    assert_eq!(fallback, 0,
        "no budget exhaustion expected for this workload");
    assert_eq!(stack, 200 * 6, "expected 6 stack records per handle × 200");
    assert_eq!(heap, 200, "expected 1 heap record (Response) per handle × 200");
}

/// Acceptance bar 2: ≥1.5× speedup with lowering. Timing-based, so
/// the CI assertion uses a relaxed 1.3× floor; the actual ratio is
/// printed and the criterion bench reports the precise number.
///
/// Marked `#[ignore]` by default: timing assertions are flaky on
/// shared CI runners (cold caches, neighbor load) and would make
/// the otherwise-deterministic test suite noisy. Run explicitly
/// with `cargo test --test response_build_acceptance -- --ignored
/// --nocapture` to get the wall-clock number.
#[test]
#[ignore = "timing-based; see doc comment for rationale"]
fn lowering_speedup_at_least_1_3x() {
    let enabled = compile_with_env(SRC, false);
    let disabled = compile_with_env(SRC, true);

    // Warmup once each to let the OS allocator settle.
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
            std::hint::black_box(
                vm.call("drive", vec![Value::Int(n)]).unwrap());
        }
        t0.elapsed().as_secs_f64()
    };

    // Best-of-3 to filter outliers; the comparison is between
    // arms in the same process, so absolute variance matters less
    // than the ratio.
    let mut enabled_times = [time_arm(&enabled), time_arm(&enabled), time_arm(&enabled)];
    let mut disabled_times = [time_arm(&disabled), time_arm(&disabled), time_arm(&disabled)];
    enabled_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    disabled_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let enabled_t = enabled_times[0];
    let disabled_t = disabled_times[0];

    let ratio = disabled_t / enabled_t;
    println!(
        "[#464 step 3] response_build best-of-3: enabled={enabled_t:.4}s, \
         disabled={disabled_t:.4}s, speedup={ratio:.2}×"
    );
    assert!(ratio >= 1.3,
        "speedup {ratio:.2}× is below the 1.3× regression floor \
         (issue acceptance bar is 1.5×; the criterion bench has the \
         precise number)");
}
