//! #463 slice 2b-i — `alloc_heavy` benchmark.
//!
//! Acceptance bars from the issue (#463):
//! - ≥ 2× speedup on an alloc-heavy bench with arena lowering
//!   enabled vs disabled.
//! - p99 on the JSON path drops measurably (no per-node `free` on
//!   the hot path).
//!
//! Workload: a handler that builds and returns one response record
//! per call, driven by a recursive loop over N calls. The returned
//! record is the **arena tier** (frame-escaping, request-local) —
//! the one case slice-2b lowering converts vs the heap baseline.
//!
//! The bench compiles the same source twice — once normally, once
//! with `LEX_NO_STACK_RECORDS=1` + `LEX_NO_ARENA_RECORDS=1` set —
//! so the A/B is on bytecode that differs only at the
//! `MakeRecord` / `AllocArenaRecord` opcode slot. All other passes
//! (peephole, IC, dispatch) run identically on both arms.
//!
//! Caveat — "deep leaves" not yet covered: slice-1 analysis is
//! pessimistic about nested aggregates (a record stored as a field
//! of another record is flagged as escaping because the outer
//! could be heap-allocated). So only the outermost returned record
//! per call is arena-routed today. Widening the analysis to hoist
//! children when their only hatch is the outer's MakeRecord is
//! future work — see `docs/design/arena-plumbing.md` § "Deep-leaf
//! trap".
//!
//! Deterministic alloc-rate is asserted by
//! `tests/alloc_heavy_acceptance.rs` (counter-driven, no wall-clock
//! noise); criterion timings are read by humans to verify the ≥ 2×
//! bar.
//!
//! Run with `cargo bench -p lex-bytecode --bench alloc_heavy`.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Program, Value};
use lex_syntax::parse_source;

/// Each call to `handle(i)` builds and returns one response record.
/// `drive(n)` calls `handle` n times via tail-recursive accumulation.
const ALLOC_HEAVY_SRC: &str = r#"
type Response = { status :: Int, total :: Int, count :: Int }

fn handle(i :: Int) -> Response {
  { status: 200, total: i * 2, count: i + 1 }
}

fn drive(n :: Int) -> Int {
  match n {
    0 => 0,
    _ => {
      let r := handle(n)
      r.total + drive(n - 1)
    },
  }
}
"#;

fn compile_with_env(src: &str, no_lowering: bool) -> Arc<Program> {
    if no_lowering {
        // SAFETY: criterion runs setup serially before timed loops.
        // Set both flags so the disabled arm is a true "no lowering"
        // baseline (the bench compares against heap MakeRecord, not
        // a partially-lowered arm).
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

fn assert_lowering_state(p: &Program, enabled: bool) {
    let handle = &p.functions[p.function_names["handle"] as usize];
    let alloc_arena = handle.code.iter()
        .filter(|op| matches!(op, lex_bytecode::Op::AllocArenaRecord { .. }))
        .count();
    let make_record = handle.code.iter()
        .filter(|op| matches!(op, lex_bytecode::Op::MakeRecord { .. }))
        .count();
    if enabled {
        assert_eq!(alloc_arena, 1,
            "expected 1 AllocArenaRecord in `handle` (enabled arm), found {alloc_arena}");
        assert_eq!(make_record, 0,
            "expected 0 MakeRecord in `handle` (enabled arm), found {make_record}");
    } else {
        assert_eq!(alloc_arena, 0,
            "expected 0 AllocArenaRecord in `handle` (disabled arm)");
        assert_eq!(make_record, 1,
            "expected 1 MakeRecord in `handle` (disabled arm)");
    }
}

fn bench_alloc_heavy(c: &mut Criterion) {
    let enabled = compile_with_env(ALLOC_HEAVY_SRC, false);
    let disabled = compile_with_env(ALLOC_HEAVY_SRC, true);
    assert_lowering_state(&enabled, true);
    assert_lowering_state(&disabled, false);

    let drive_id_enabled = enabled.function_names["drive"];
    let drive_id_disabled = disabled.function_names["drive"];

    let mut group = c.benchmark_group("alloc_heavy");
    for n in [100u32, 1000u32].iter().copied() {
        group.throughput(Throughput::Elements(n as u64));

        group.bench_function(format!("enabled/n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&enabled);
                let scope = vm.enter_request_scope();
                let r = vm.invoke(drive_id_enabled, vec![Value::Int(n as i64)]).unwrap();
                black_box(vm.materialize_arena_handles(r));
                vm.exit_request_scope(scope);
            });
        });

        group.bench_function(format!("disabled/n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&disabled);
                // No scope on the disabled arm — there's nothing for
                // it to do, every alloc is a heap MakeRecord.
                let r = vm.invoke(drive_id_disabled, vec![Value::Int(n as i64)]).unwrap();
                black_box(r);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_alloc_heavy);
criterion_main!(benches);
