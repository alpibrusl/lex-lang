//! Steady-state JIT vs interpreter micro-benchmark for the MVP op set.
//!
//! Runs the same hand-crafted Function — a `sum_from_1_to_n` loop
//! built from `LoadLocal` / `StoreLocal` / `IntAdd` / `IntLe` /
//! `Jump` / `JumpIfNot` — through both paths and reports ns per
//! call. The function is compiled / `Vm` is constructed once,
//! *outside* the iter loop, so the numbers exclude:
//!
//! - JIT compile time (Cranelift codegen). For a single one-shot
//!   call the JIT loses by milliseconds; only the steady-state
//!   ratio is meaningful at this stage, before tier-up integration
//!   amortizes compile cost over many invocations.
//! - `Program` build + `Vm::new`. Captured once and reused per
//!   iter — same shape as `crates/lex-bytecode/benches/dispatch.rs`.
//!
//! What this bench shows is the **lower bound** for JIT ROI on
//! Lex's hot path: pure-int arithmetic in a loop, no boxed `Value`
//! traffic, no records or closures. Real Lex programs will land
//! somewhere between this and 1× (per the roadmap, until the
//! value-rep / NaN-boxing decision lands).
//!
//! Run with `cargo bench -p lex-jit --features cranelift --bench jit_vs_interp`.
//! The `required-features = ["cranelift"]` gate in `Cargo.toml` means
//! cargo silently skips this bench when the feature is off, so the
//! file body assumes cranelift is on.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use indexmap::IndexMap;
use lex_bytecode::op::{Const, Op};
use lex_bytecode::program::{Function, Program, ZERO_BODY_HASH};
use lex_bytecode::value::Value;
use lex_bytecode::vm::Vm;
use lex_jit::{JitContext, JitVm};

/// Build the loop `fn f(n :: Int) -> Int { let acc=0, i=1; while i<=n
/// { acc+=i; i+=1 }; acc }` — the same bytecode the JIT MVP test
/// `sum_from_one_to_n_via_loop` covers. Op set is entirely in the
/// MVP-supported subset.
fn sum_loop_program() -> Program {
    let code = vec![
        Op::PushConst(0), // const 0 = 1
        Op::StoreLocal(1),
        Op::PushConst(1), // const 1 = 0
        Op::StoreLocal(2),
        Op::LoadLocal(1),
        Op::LoadLocal(0),
        Op::IntLe,
        Op::JumpIfNot(9),
        Op::LoadLocal(2),
        Op::LoadLocal(1),
        Op::IntAdd,
        Op::StoreLocal(2),
        Op::LoadLocal(1),
        Op::PushConst(0),
        Op::IntAdd,
        Op::StoreLocal(1),
        Op::Jump(-13),
        Op::LoadLocal(2),
        Op::Return,
    ];
    let mut function_names = IndexMap::new();
    function_names.insert("f".to_string(), 0);
    Program {
        constants: vec![Const::Int(1), Const::Int(0)],
        functions: vec![Function {
            name: "f".into(),
            arity: 1,
            locals_count: 3,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        }],
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![],
    }
}

/// Straight-line int polynomial — no jumps, no per-iter dispatch
/// loop in the bytecode. Tests the dispatch-floor case: the
/// interpreter ought to be near its limit here (every op is a
/// trivial-handler arm), so the JIT win is purely from removing
/// the dispatch loop and unboxing values.
fn polynomial_program() -> Program {
    // fn f(a, b, c) -> Int { a*a + b*b + c*c + a*b + b*c }
    let code = vec![
        Op::LoadLocal(0),
        Op::LoadLocal(0),
        Op::IntMul, // a*a
        Op::LoadLocal(1),
        Op::LoadLocal(1),
        Op::IntMul, // b*b
        Op::IntAdd, // a*a + b*b
        Op::LoadLocal(2),
        Op::LoadLocal(2),
        Op::IntMul, // c*c
        Op::IntAdd, // a*a + b*b + c*c
        Op::LoadLocal(0),
        Op::LoadLocal(1),
        Op::IntMul, // a*b
        Op::IntAdd,
        Op::LoadLocal(1),
        Op::LoadLocal(2),
        Op::IntMul, // b*c
        Op::IntAdd,
        Op::Return,
    ];
    let mut function_names = IndexMap::new();
    function_names.insert("f".to_string(), 0);
    Program {
        constants: vec![],
        functions: vec![Function {
            name: "f".into(),
            arity: 3,
            locals_count: 3,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        }],
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![],
    }
}

fn bench_sum_loop(c: &mut Criterion) {
    let prog = sum_loop_program();
    let mut ctx = JitContext::new().expect("jit ctx");
    let jitted = ctx
        .compile(&prog.functions[0], &prog.constants)
        .expect("jit compile");

    let mut group = c.benchmark_group("jit_vs_interp/sum_loop");
    for &n in &[100i64, 1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(n as u64));

        group.bench_function(format!("interp/n={n}"), |b| {
            let mut vm = Vm::new(&prog);
            vm.set_step_limit(u64::MAX);
            b.iter(|| {
                black_box(
                    vm.call("f", vec![Value::Int(black_box(n))])
                        .expect("interp call"),
                )
            });
        });

        group.bench_function(format!("jit_raw/n={n}"), |b| {
            b.iter(|| black_box(unsafe { jitted.call(&[black_box(n)]) }));
        });

        // The tiered path — `JitVm::call`. Includes the wrapper's
        // per-call overhead (lookup, cache hit, arg unbox, result
        // re-box). Measures what a real caller of the public API
        // would actually see, vs the raw fn-pointer call above.
        group.bench_function(format!("jit_vm/n={n}"), |b| {
            let mut jitvm = JitVm::new(&prog).expect("JitVm::new");
            // Prime the cache so we're measuring steady-state.
            jitvm
                .call("f", vec![Value::Int(n)])
                .expect("prime jit_vm cache");
            b.iter(|| {
                black_box(
                    jitvm
                        .call("f", vec![Value::Int(black_box(n))])
                        .expect("jit_vm call"),
                )
            });
        });
    }
    group.finish();
}

fn bench_polynomial(c: &mut Criterion) {
    let prog = polynomial_program();
    let mut ctx = JitContext::new().expect("jit ctx");
    let jitted = ctx
        .compile(&prog.functions[0], &prog.constants)
        .expect("jit compile");

    let mut group = c.benchmark_group("jit_vs_interp/polynomial");

    group.bench_function("interp", |b| {
        let mut vm = Vm::new(&prog);
        vm.set_step_limit(u64::MAX);
        b.iter(|| {
            black_box(
                vm.call(
                    "f",
                    vec![
                        Value::Int(black_box(7)),
                        Value::Int(black_box(11)),
                        Value::Int(black_box(13)),
                    ],
                )
                .expect("interp call"),
            )
        });
    });

    group.bench_function("jit_raw", |b| {
        b.iter(|| {
            black_box(unsafe {
                jitted.call(&[black_box(7), black_box(11), black_box(13)])
            })
        });
    });

    group.bench_function("jit_vm", |b| {
        let mut jitvm = JitVm::new(&prog).expect("JitVm::new");
        jitvm
            .call(
                "f",
                vec![Value::Int(7), Value::Int(11), Value::Int(13)],
            )
            .expect("prime jit_vm cache");
        b.iter(|| {
            black_box(
                jitvm
                    .call(
                        "f",
                        vec![
                            Value::Int(black_box(7)),
                            Value::Int(black_box(11)),
                            Value::Int(black_box(13)),
                        ],
                    )
                    .expect("jit_vm call"),
            )
        });
    });

    group.finish();
}

/// Outer loop that calls an inner eligible function many times.
/// This is the shape `Op::Call` interception was built for:
///
///   `outer(n)` is ineligible (contains a `MakeTuple`), so the
///   *outer* call lands on the interpreter; but its tight loop
///   calls `square(i)` via `Op::Call`, and `square` *is*
///   eligible. The JIT tier intercepts each inner call.
fn sum_of_squares_program() -> Program {
    let outer = Function {
        name: "outer".into(),
        arity: 1,
        locals_count: 3,
        code: vec![
            Op::PushConst(0),            // acc = 0
            Op::StoreLocal(1),
            Op::PushConst(0),            // i = 0
            Op::StoreLocal(2),
            Op::LoadLocal(2),            // loop:
            Op::LoadLocal(0),
            Op::IntLt,
            Op::JumpIfNot(10),           // -> after-loop block at pc 18
            Op::LoadLocal(1),
            Op::LoadLocal(2),
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 2 },
            Op::IntAdd,
            Op::StoreLocal(1),
            Op::LoadLocal(2),
            Op::PushConst(1),
            Op::IntAdd,
            Op::StoreLocal(2),
            Op::Jump(-14),               // -> pc 4
            Op::PushConst(0),            // dummy tuple to keep `outer` ineligible
            Op::PushConst(0),
            Op::MakeTuple(2),
            Op::Pop,
            Op::LoadLocal(1),
            Op::Return,
        ],
        effects: vec![],
        body_hash: ZERO_BODY_HASH,
        refinements: vec![],
        field_ic_sites: 0,
    };
    let square = Function {
        name: "square".into(),
        arity: 1,
        locals_count: 1,
        code: vec![
            Op::LoadLocal(0),
            Op::LoadLocal(0),
            Op::IntMul,
            Op::Return,
        ],
        effects: vec![],
        body_hash: ZERO_BODY_HASH,
        refinements: vec![],
        field_ic_sites: 0,
    };
    let mut function_names = IndexMap::new();
    function_names.insert("outer".to_string(), 0);
    function_names.insert("square".to_string(), 1);
    Program {
        constants: vec![
            Const::Int(0),
            Const::Int(1),
            Const::NodeId("outer_calls_square".into()),
        ],
        functions: vec![outer, square],
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![],
    }
}

fn bench_sum_of_squares(c: &mut Criterion) {
    let prog = sum_of_squares_program();
    let mut group = c.benchmark_group("jit_vs_interp/sum_of_squares");

    for &n in &[100i64, 1_000, 10_000] {
        group.throughput(Throughput::Elements(n as u64));

        // Interpreter: outer interpreted, every `square(i)` also
        // interpreted via Op::Call frame setup.
        group.bench_function(format!("interp/n={n}"), |b| {
            let mut vm = Vm::new(&prog);
            vm.set_step_limit(u64::MAX);
            b.iter(|| {
                black_box(
                    vm.call("outer", vec![Value::Int(black_box(n))])
                        .expect("interp call"),
                )
            });
        });

        // jit_vm: outer interpreted (it's ineligible), but each
        // `square(i)` dispatches through the JIT via the hook.
        // This is the Op::Call interception story.
        group.bench_function(format!("jit_vm/n={n}"), |b| {
            let mut jitvm = JitVm::new(&prog).expect("JitVm::new");
            jitvm
                .call("outer", vec![Value::Int(n)])
                .expect("prime cache");
            b.iter(|| {
                black_box(
                    jitvm
                        .call("outer", vec![Value::Int(black_box(n))])
                        .expect("jit_vm call"),
                )
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_sum_loop, bench_polynomial, bench_sum_of_squares);
criterion_main!(benches);
