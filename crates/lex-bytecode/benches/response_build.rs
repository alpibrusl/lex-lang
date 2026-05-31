//! #464 step 3 — `response_build` benchmark.
//!
//! Acceptance bars from the issue:
//! - ≥1.5× speedup with the escape-driven lowering enabled vs
//!   disabled.
//! - ≥60% of record allocations on the stack (i.e. the
//!   `AllocStackRecord` stack path, not the budget fallback or
//!   `MakeRecord` heap path).
//!
//! Workload: a handler-shaped function that builds several
//! intermediate records (validation, pricing, summary) read
//! field-only and discarded, then constructs and returns one
//! response record. The intermediates are all non-escaping per the
//! escape analysis; the returned response is the heap baseline.
//!
//! The bench compiles the same source twice — once normally, once
//! with `LEX_NO_STACK_RECORDS=1` set during compilation — so the
//! A/B comparison is on bytecode that differs only at the
//! `MakeRecord` / `AllocStackRecord` opcode slot. All other passes
//! (peephole, IC, dispatch) run identically on both arms.
//!
//! The stack-allocation rate is measured separately via the per-VM
//! counters `Vm::stack_record_allocs` / `heap_record_allocs` /
//! `stack_record_heap_fallbacks` and asserted by an accompanying
//! test (`tests/response_build_acceptance.rs`); criterion timings
//! are read by humans to verify the ≥1.5× bar.
//!
//! Run with `cargo bench -p lex-bytecode --bench response_build`.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Program, Value};
use lex_syntax::parse_source;

/// Handler workload. Mirrors the layered-intermediate pattern that
/// the typical Lex web handler exhibits — destructure request,
/// build a chain of computation records each consuming the
/// previous, branch on a derived flag, return a response record.
/// Six non-escaping intermediates (v1..v6) plus one escaping
/// Response, so the stack-allocation rate is 6/7 ≈ 85.7%.
///
/// Argument list is flat (scalars, not a Request record) so the
/// bench isolates the local-record allocation cost from per-call
/// argument-passing noise.
const RESPONSE_BUILD_SRC: &str = r#"
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
        // SAFETY: bench is single-threaded; criterion drives the
        // closure sequentially within each `bench_function`. Set
        // the var, compile, unset, return.
        // SAFETY note (#464 step 3): `std::env::set_var` on Linux
        // requires no other thread is concurrently reading
        // environment variables. Criterion's harness is
        // single-threaded inside a measurement; we set the env var
        // only during compilation (well before the timed loop) and
        // unset it immediately after.
        // The bench wrapper compiles BOTH arms once at startup
        // (outside the timed loop), so the set/unset window is
        // brief and inside the benchmark setup phase.
        // SAFETY discussion is moot if we ran the code in a
        // subprocess, but criterion-level subprocess overhead
        // would dwarf the signal. We accept the unsafe (also gated
        // on the bench-only `compile_with_env` helper).
        // Slice 2b-i: also suppress arena lowering so the disabled
        // arm A/B is a true "no lowering" baseline, not a partial one.
        unsafe { std::env::set_var("LEX_NO_STACK_RECORDS", "1"); }
        unsafe { std::env::set_var("LEX_NO_ARENA_RECORDS", "1"); }
        let prog = parse_source(src).expect("parse");
        let stages = canonicalize_program(&prog);
        lex_types::check_program(&stages).expect("typecheck");
        let p = Arc::new(compile_program(&stages));
        unsafe { std::env::remove_var("LEX_NO_STACK_RECORDS"); }
        unsafe { std::env::remove_var("LEX_NO_ARENA_RECORDS"); }
        p
    } else {
        let prog = parse_source(src).expect("parse");
        let stages = canonicalize_program(&prog);
        lex_types::check_program(&stages).expect("typecheck");
        Arc::new(compile_program(&stages))
    }
}

/// Sanity check that the workload's record sites actually got
/// lowered on the "enabled" arm and were left alone on the
/// "disabled" arm. A regression that quietly turned off the pass
/// would produce equal timings; assert here so the bench fails
/// loudly instead of misleading.
fn assert_lowering_state(p: &Program, expect_lowered: bool) {
    use lex_bytecode::Op;
    let mut total_record_sites = 0usize;
    let mut lowered_sites = 0usize;
    for f in &p.functions {
        for op in &f.code {
            match op {
                Op::MakeRecord { .. } => total_record_sites += 1,
                Op::AllocStackRecord { .. } => {
                    total_record_sites += 1;
                    lowered_sites += 1;
                }
                _ => {}
            }
        }
    }
    assert!(total_record_sites > 0, "workload must have record sites");
    if expect_lowered {
        assert!(lowered_sites > 0,
            "expected lowering on enabled arm (sites: {total_record_sites})");
    } else {
        assert_eq!(lowered_sites, 0,
            "expected no lowering on disabled arm (sites: {total_record_sites})");
    }
}

fn bench_response_build(c: &mut Criterion) {
    let enabled = compile_with_env(RESPONSE_BUILD_SRC, false);
    let disabled = compile_with_env(RESPONSE_BUILD_SRC, true);
    assert_lowering_state(&enabled, true);
    assert_lowering_state(&disabled, false);

    let mut group = c.benchmark_group("response_build");
    // Each iteration runs the handler for n requests. Two arms
    // compared inside the same group so criterion prints them
    // side-by-side; the eyeball check is `enabled / disabled ≤ 1/1.5`.
    for n in [100i64, 1_000] {
        let nu = n as u64;
        // Each handle() call builds 6 stack records + 1 heap
        // record = 7 record allocations. Throughput::Elements
        // wired to that lets criterion print alloc/sec.
        group.throughput(Throughput::Elements(7 * nu));

        let enabled_arm = Arc::clone(&enabled);
        group.bench_function(format!("enabled/n={n}"), move |b| {
            b.iter(|| {
                let mut vm = Vm::new(&enabled_arm);
                vm.set_step_limit(u64::MAX);
                black_box(vm.call("drive", vec![Value::Int(n)]).unwrap());
            })
        });

        let disabled_arm = Arc::clone(&disabled);
        group.bench_function(format!("disabled/n={n}"), move |b| {
            b.iter(|| {
                let mut vm = Vm::new(&disabled_arm);
                vm.set_step_limit(u64::MAX);
                black_box(vm.call("drive", vec![Value::Int(n)]).unwrap());
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_response_build);
criterion_main!(benches);
