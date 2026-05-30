//! #464 tuple codegen — `tuple_build` benchmark.
//!
//! Tuple analogue of `response_build`: a handler-shaped function that
//! builds and destructures several non-escaping intermediate tuples,
//! returning a scalar. Compiles the same source twice — normally and
//! with `LEX_NO_STACK_RECORDS=1` (which gates the whole escape
//! lowering, tuples included) — so the A/B differs only at the
//! `MakeTuple` / `AllocStackTuple` opcode slot.
//!
//! The deterministic instruction count comes from the callgrind
//! harness (`examples/profile_tuple_build.rs`); the stack-allocation
//! count and a relaxed timing floor are asserted by
//! `tests/tuple_build_acceptance.rs`. Criterion timings here are read
//! by humans.
//!
//! Run with `cargo bench -p lex-bytecode --bench tuple_build`.

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Program, Value};
use lex_syntax::parse_source;

/// Six non-escaping intermediate tuples per `handle`, each built and
/// destructured via `match`. `drive` passes/reads scalars, so no
/// tuple escapes — every intermediate lands on the stack path.
const TUPLE_BUILD_SRC: &str = r#"
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
    // SAFETY: bench is single-threaded; the env var is set only during
    // compilation (outside the timed loop) and unset immediately after.
    if no_stack {
        unsafe { std::env::set_var("LEX_NO_STACK_RECORDS", "1"); }
    }
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let p = Arc::new(compile_program(&stages));
    if no_stack {
        unsafe { std::env::remove_var("LEX_NO_STACK_RECORDS"); }
    }
    p
}

/// Fail loudly if the pass quietly stopped firing (which would make
/// the two arms identical and the comparison meaningless).
fn assert_lowering_state(p: &Program, expect_lowered: bool) {
    use lex_bytecode::Op;
    let mut total = 0usize;
    let mut lowered = 0usize;
    for f in &p.functions {
        for op in &f.code {
            match op {
                Op::MakeTuple(_) => total += 1,
                Op::AllocStackTuple { .. } => { total += 1; lowered += 1; }
                _ => {}
            }
        }
    }
    assert!(total > 0, "workload must have tuple sites");
    if expect_lowered {
        assert!(lowered > 0, "expected lowering on enabled arm");
    } else {
        assert_eq!(lowered, 0, "expected no lowering on disabled arm");
    }
}

fn bench_tuple_build(c: &mut Criterion) {
    let enabled = compile_with_env(TUPLE_BUILD_SRC, false);
    let disabled = compile_with_env(TUPLE_BUILD_SRC, true);
    assert_lowering_state(&enabled, true);
    assert_lowering_state(&disabled, false);

    let mut group = c.benchmark_group("tuple_build");
    for n in [100i64, 1_000] {
        let nu = n as u64;
        // 6 tuple allocations per handle() call.
        group.throughput(Throughput::Elements(6 * nu));

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

criterion_group!(benches, bench_tuple_build);
criterion_main!(benches);
