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
    bench_straight_arith_no_dispatch,
    bench_pure_dispatch,
    bench_two_local_arith,
    bench_two_local_sub_arith,
    bench_two_local_mul_arith,
);
criterion_main!(benches);

/// Build a `two_local_arith` source: each let-binding sums two
/// already-bound locals (no constant operand), so each compiles to
/// `LoadLocal + LoadLocal + IntAdd + StoreLocal`. The first two slots
/// are exactly slice 3's pattern (`LoadLocalAddLocal`) and the trailing
/// `StoreLocal` would be slice 3's natural follow-on if it ever grew
/// a `StoreLocal`-absorbing companion (mirroring slice 2's relation
/// to slice 1). Sibling to `straight_arith` — same shape, but with a
/// LoadLocal as the second operand instead of a PushConst, so slice 3
/// is the only superinstruction that fires here.
fn make_two_local_arith_source(n: usize) -> String {
    // a0 = start, a1 = start, a{i} = a{i-1} + a{i-2}-equivalent
    // pattern (the second-operand local cycles between the two
    // seeds so we don't overflow into a useless big number — we
    // just want consistent LoadLocal+LoadLocal+IntAdd dispatch shape).
    let mut s = String::with_capacity(48 * n);
    s.push_str("fn two_local_arith(start :: Int, step :: Int) -> Int {\n");
    s.push_str("  let a0 := start\n");
    for i in 1..=n {
        // Always: a{i} := a{i-1} + step. Both operands are Int locals,
        // so compile_binop emits IntAdd (typed lowering) and the
        // peephole slice 3 fuses the LoadLocal+LoadLocal+IntAdd triple.
        s.push_str(&format!("  let a{i} := a{} + step\n", i - 1));
    }
    s.push_str(&format!("  a{n}\n"));
    s.push_str("}\n");
    s
}

fn bench_two_local_arith(c: &mut Criterion) {
    let mut group = c.benchmark_group("dispatch/two_local_arith");
    for n in [100usize, 1_000, 5_000] {
        let prog = compile(&make_two_local_arith_source(n));
        // 4 dispatch steps per let-binding without superinstructions:
        // LoadLocal + LoadLocal + IntAdd + StoreLocal. With slice 3 the
        // first three collapse to one dispatched op (the trailing two
        // are tombstones the VM steps past in one `pc + 3` update).
        // Throughput count uses the unfused-equivalent step count so
        // ns/elem numbers are comparable to `straight_arith`.
        group.throughput(Throughput::Elements((4 * n) as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("two_local_arith", vec![Value::Int(0), Value::Int(1)])
                        .expect("call two_local_arith"),
                )
            })
        });
    }
    group.finish();
}

/// Sibling of `bench_two_local_arith` for the `IntSub` shape
/// (#461 slice 4). Same source pattern but with `-` instead of `+`,
/// so each let-binding compiles to `LoadLocal + LoadLocal + IntSub +
/// StoreLocal`; slice 4 collapses the first three into
/// `LoadLocalSubLocal`. Isolated bench so the slice-4 dispatch saving
/// is the only superinstruction that fires.
fn bench_two_local_sub_arith(c: &mut Criterion) {
    let mut group = c.benchmark_group("dispatch/two_local_sub_arith");
    for n in [100usize, 1_000, 5_000] {
        let prog = compile(&make_two_local_binop_source(n, "-"));
        group.throughput(Throughput::Elements((4 * n) as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("two_local_binop", vec![Value::Int(0), Value::Int(0)])
                        .expect("call two_local_binop (-)"),
                )
            })
        });
    }
    group.finish();
}

/// Sibling of `bench_two_local_arith` for the `IntMul` shape
/// (#461 slice 4). Seeded with `1, 1` so the running product stays
/// `1` and we don't overflow — the dispatch shape is the same
/// regardless of the values, and that's what we're measuring.
fn bench_two_local_mul_arith(c: &mut Criterion) {
    let mut group = c.benchmark_group("dispatch/two_local_mul_arith");
    for n in [100usize, 1_000, 5_000] {
        let prog = compile(&make_two_local_binop_source(n, "*"));
        group.throughput(Throughput::Elements((4 * n) as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("two_local_binop", vec![Value::Int(1), Value::Int(1)])
                        .expect("call two_local_binop (*)"),
                )
            })
        });
    }
    group.finish();
}

/// Generalised version of `make_two_local_arith_source` for slice 4
/// — emits the same chain shape (each `a{i}` := previous-local OP
/// step), parameterised on the binary operator string.
fn make_two_local_binop_source(n: usize, op: &str) -> String {
    let mut s = String::with_capacity(48 * n);
    s.push_str("fn two_local_binop(start :: Int, step :: Int) -> Int {\n");
    s.push_str("  let a0 := start\n");
    for i in 1..=n {
        s.push_str(&format!("  let a{i} := a{} {op} step\n", i - 1));
    }
    s.push_str(&format!("  a{n}\n"));
    s.push_str("}\n");
    s
}

/// Pure-dispatch microbench: hand-built `Program` whose body is N
/// `PushConst(Unit) + Pop` pairs followed by `PushConst(Int 0) +
/// Return`. Each pair is two dispatches over the cheapest possible
/// arm bodies (one `Vec::push(Value::Unit)`, one `Vec::pop()`). The
/// reported `ns/elem` (with throughput = `2 * n`) bounds dispatch
/// overhead from above — any real arm body does strictly more work
/// per step. Compare against `straight_arith` (4.7 ns/elem with
/// superinstructions) to estimate what fraction of `straight_arith`
/// is dispatch vs arm work.
fn bench_pure_dispatch(c: &mut Criterion) {
    use lex_bytecode::op::{Const, Op};
    use lex_bytecode::program::{Function, ZERO_BODY_HASH};
    use indexmap::IndexMap;

    let mut group = c.benchmark_group("dispatch/pure");
    for n in [1_000usize, 10_000, 100_000] {
        let mut code = Vec::with_capacity(2 * n + 2);
        // Constants: [Unit, Int(0)]
        let constants = vec![Const::Unit, Const::Int(0)];
        for _ in 0..n {
            code.push(Op::PushConst(0)); // push Unit
            code.push(Op::Pop);
        }
        code.push(Op::PushConst(1)); // push Int(0)
        code.push(Op::Return);

        let func = Function {
            name: "pure_dispatch".to_string(),
            arity: 0,
            locals_count: 0,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        };
        let mut function_names = IndexMap::new();
        function_names.insert("pure_dispatch".to_string(), 0);
        let prog = Arc::new(Program {
            constants,
            functions: vec![func],
            function_names,
            module_aliases: IndexMap::new(),
            entry: Some(0),
            record_shapes: vec![],
        });

        group.throughput(Throughput::Elements((2 * n) as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(
                    vm.call("pure_dispatch", vec![])
                        .expect("call pure_dispatch"),
                )
            })
        });
    }
    group.finish();
}
