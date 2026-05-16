//! Microbenchmarks for the bytecode VM's dispatch loop (#461).
//!
//! Establishes a baseline against which the planned dispatch rewrite
//! (function-table / computed-goto) is measured. Four workloads stress
//! different shapes of the hot path:
//!
//! - `arith_loop` — tight integer arithmetic via tail-recursive `sum_to`.
//!   Dense mix of trivial-handler opcodes plus per-call setup.
//! - `record_field` — repeated `GetField` over the same record shape.
//!   Stresses the inline-cache hot path that #462 will rework.
//! - `call_heavy` — recursive factorial. Isolates Call/Return overhead
//!   relative to per-call work done.
//! - `straight_arith` — a function whose body is a long flat chain of
//!   `let aN := a_{N-1} + 1` bindings. No recursion, no calls inside
//!   the body — just dispatch over a known opcode mix per iteration
//!   (LoadLocal + PushConst + IntAdd + StoreLocal). The cleanest signal
//!   for dispatch-loop cost: dividing bench time by the reported
//!   throughput element count gives ns per dispatch step with
//!   negligible contamination from call setup or branchy bodies.
//!
//! Run with `cargo bench -p lex-bytecode --bench dispatch`.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Program, Value};
use lex_syntax::parse_source;

const ARITH_LOOP_SRC: &str = r#"
fn sum_to(n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_to(n - 1, acc + n),
  }
}
"#;

const RECORD_FIELD_SRC: &str = r#"
type Point = { x :: Int, y :: Int, z :: Int }

fn sum_fields(p :: Point, n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_fields(p, n - 1, acc + p.x + p.y + p.z),
  }
}

fn bench(n :: Int) -> Int {
  let p :: Point := { x: 1, y: 2, z: 3 }
  sum_fields(p, n, 0)
}
"#;

const CALL_HEAVY_SRC: &str = r#"
fn factorial(n :: Int) -> Int {
  match n {
    0 => 1,
    _ => n * factorial(n - 1),
  }
}
"#;

fn compile(src: &str) -> Arc<Program> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    Arc::new(compile_program(&stages))
}

fn bench_arith_loop(c: &mut Criterion) {
    let prog = compile(ARITH_LOOP_SRC);
    let mut group = c.benchmark_group("dispatch/arith_loop");
    for n in [100i64, 1_000, 10_000] {
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("sum_to", vec![Value::Int(n), Value::Int(0)])
                        .expect("call sum_to"),
                )
            })
        });
    }
    group.finish();
}

fn bench_record_field(c: &mut Criterion) {
    let prog = compile(RECORD_FIELD_SRC);
    let mut group = c.benchmark_group("dispatch/record_field");
    for n in [100i64, 1_000, 10_000] {
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("bench", vec![Value::Int(n)])
                        .expect("call bench"),
                )
            })
        });
    }
    group.finish();
}

fn bench_call_heavy(c: &mut Criterion) {
    let prog = compile(CALL_HEAVY_SRC);
    let mut group = c.benchmark_group("dispatch/call_heavy");
    for n in [10i64, 15, 20] {
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("factorial", vec![Value::Int(n)])
                        .expect("call factorial"),
                )
            })
        });
    }
    group.finish();
}

/// Build a `straight_arith` source with `n` flat let-bindings.
///
/// Each binding compiles to roughly LoadLocal + PushConst + IntAdd +
/// StoreLocal (4 dispatch steps). The function has no recursion and
/// no internal calls, so once `straight_arith` is invoked from the
/// host the VM spends essentially all of its time in the dispatch
/// loop — no Call/Return setup, no allocations, no IC lookups.
///
/// We report throughput as `4 * n` elements so criterion prints
/// elements/sec (≈ dispatch steps/sec); dividing wall time by the
/// element count gives ns per dispatch step directly.
fn make_straight_arith_source(n: usize) -> String {
    let mut s = String::with_capacity(40 * n);
    s.push_str("fn straight_arith(start :: Int) -> Int {\n");
    s.push_str("  let a0 := start\n");
    for i in 1..=n {
        s.push_str(&format!("  let a{i} := a{} + 1\n", i - 1));
    }
    s.push_str(&format!("  a{n}\n"));
    s.push_str("}\n");
    s
}

fn bench_straight_arith(c: &mut Criterion) {
    let mut group = c.benchmark_group("dispatch/straight_arith");
    for n in [100usize, 1_000, 5_000] {
        let prog = compile(&make_straight_arith_source(n));
        // 4 dispatch steps per let-binding: LoadLocal + PushConst +
        // IntAdd + StoreLocal. The trailing `a{n}` adds one
        // LoadLocal + Return; absorbed into the per-iter constant.
        group.throughput(Throughput::Elements((4 * n) as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("straight_arith", vec![Value::Int(0)])
                        .expect("call straight_arith"),
                )
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_arith_loop,
    bench_record_field,
    bench_call_heavy,
    bench_straight_arith
);
criterion_main!(benches);
