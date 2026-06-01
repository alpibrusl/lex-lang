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
use lex_jit::JitContext;

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

        group.bench_function(format!("jit/n={n}"), |b| {
            b.iter(|| black_box(unsafe { jitted.call(&[black_box(n)]) }));
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

    group.bench_function("jit", |b| {
        b.iter(|| {
            black_box(unsafe {
                jitted.call(&[black_box(7), black_box(11), black_box(13)])
            })
        });
    });
    group.finish();
}

criterion_group!(benches, bench_sum_loop, bench_polynomial);
criterion_main!(benches);
