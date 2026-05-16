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

/// Lower-bound bench: the same work as `straight_arith` (LoadLocal +
/// PushConst + IntAdd + StoreLocal × n) without any bytecode dispatch.
/// Runs as inline Rust over a `Vec<Value>` stack and a `Vec<Value>`
/// locals array, mirroring the storage shapes the real VM uses.
///
/// This is an *intervention diagnostic*: the gap between this and
/// `dispatch/straight_arith` is everything the bytecode interpreter
/// adds on top of the actual semantic work (dispatch decode, frame
/// indexing, step-limit check, op-pc advance, etc.). If the gap is
/// small, dispatch tuning has little headroom and #461 should pivot
/// to something else (frame caching, stack-storage rework, ...).
/// If it's large, structural dispatch rewrites are justified.
///
/// Caveat: this bench's Value path matches the real VM's (clone on
/// Load, push on PushConst, pop+pop+IntAdd push on IntAdd, pop+store
/// on StoreLocal). It does *not* model the Vm's `step_limit` or
/// `steps += 1` counter. Subtract those if you want the absolute
/// floor — the dispatch loop's own overhead is what we're isolating
/// here.
fn bench_straight_arith_no_dispatch(c: &mut Criterion) {
    use lex_bytecode::Value;
    let mut group = c.benchmark_group("dispatch/straight_arith_no_dispatch");
    for n in [100usize, 1_000, 5_000] {
        group.throughput(Throughput::Elements((4 * n) as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                // One local slot per `aN`. Matches the real VM's
                // `locals_storage` shape: a flat Vec<Value> indexed
                // by `base + local_idx`.
                let mut locals: Vec<Value> = vec![Value::Int(0); n + 1];
                let mut stack: Vec<Value> = Vec::with_capacity(8);
                locals[0] = Value::Int(black_box(0));
                for i in 1..=n {
                    // LoadLocal(i-1)
                    stack.push(locals[i - 1].clone());
                    // PushConst(1) — constant pool lookup elided since
                    // the real VM also just clones the Const into a
                    // Value; the work shape is the same.
                    stack.push(Value::Int(1));
                    // IntAdd
                    let b = stack.pop().unwrap();
                    let a = stack.pop().unwrap();
                    let r = match (a, b) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x + y),
                        _ => unreachable!(),
                    };
                    stack.push(r);
                    // StoreLocal(i)
                    locals[i] = stack.pop().unwrap();
                }
                // Return locals[n]
                black_box(locals[n].clone())
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
    bench_straight_arith,
    bench_straight_arith_no_dispatch
);
criterion_main!(benches);
