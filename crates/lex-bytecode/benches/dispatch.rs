//! Microbenchmarks for the bytecode VM's dispatch loop (#461).
//!
//! Establishes a baseline against which the planned dispatch rewrite
//! (function-table / computed-goto) is measured. Three workloads stress
//! different shapes of the hot path:
//!
//! - `arith_loop` — tight integer arithmetic via tail-recursive `sum_to`.
//!   Dense mix of trivial-handler opcodes: PushConst, IntAdd, IntSub,
//!   LoadLocal/StoreLocal, Call, Return, match-arm jumps.
//! - `record_field` — repeated `GetField` over the same record shape.
//!   Stresses the inline-cache hot path that #462 will rework.
//! - `call_heavy` — recursive factorial. Stresses Call/Return overhead
//!   relative to the per-call work done.
//!
//! Run with `cargo bench -p lex-bytecode --bench dispatch`.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
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

criterion_group!(
    benches,
    bench_arith_loop,
    bench_record_field,
    bench_call_heavy
);
criterion_main!(benches);
