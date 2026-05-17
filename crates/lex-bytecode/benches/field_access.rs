//! Microbenchmarks for `Op::GetField` — the record-access hot path
//! that #462's polymorphic inline cache reworks.
//!
//! The existing `dispatch.rs::record_field` bench mixes field access
//! with tail-recursion overhead, so per-access cost is muddied by
//! call-frame setup. This bench isolates field access by running a
//! flat (non-recursive) chain of `GetField` ops inside a single
//! function body, in three flavors:
//!
//! - `mono_first` — read field 0 of a 4-field record N times. Tests
//!   the IC hit-rate on the cheapest possible access (first field
//!   in the `IndexMap`).
//! - `mono_last` — read the last field of an 8-field record N times.
//!   Tests miss-path cost (hash walk that today scans the field list
//!   linearly). Today's monomorphic IC hides this on hot loops, but
//!   the absolute miss cost still matters for cold paths.
//! - `mono_chain` — read x + y + z of a 3-field record per iteration,
//!   inside a tail-recursive loop. Same as `dispatch.rs::record_field`
//!   but kept here as the apples-to-apples #462 baseline that's
//!   directly comparable to the issue's "≥ 3×" acceptance.
//!
//! All three workloads are monomorphic — the IC sees one shape per
//! call site. The polymorphic / megamorphic cases are queued for a
//! follow-up once #462's shape_id propagation (slice 3) lands; right
//! now Lex's type checker enforces a single record type per
//! `GetField` site, so synthetically constructing a polymorphic site
//! requires the shape-ID changes.
//!
//! Run with `cargo bench -p lex-bytecode --bench field_access`.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Program, Value};
use lex_syntax::parse_source;

fn compile(src: &str) -> Arc<Program> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    Arc::new(compile_program(&stages))
}

/// Build a function body that reads `field` of a record `n` times in
/// a flat chain, summing into an accumulator. No recursion — straight
/// chain of `LoadLocal(p) + GetField(field) + IntAdd` per step.
fn make_flat_get_field(field: &str, type_decl: &str, ctor: &str, n: usize) -> String {
    let mut body = String::new();
    body.push_str(type_decl);
    body.push_str("\n\nfn bench() -> Int {\n");
    body.push_str(&format!("  let p :: R := {ctor}\n"));
    body.push_str("  let a0 := 0\n");
    for i in 1..=n {
        body.push_str(&format!("  let a{i} := a{prev} + p.{field}\n", prev = i - 1));
    }
    body.push_str(&format!("  a{n}\n}}\n"));
    body
}

fn bench_mono_first(c: &mut Criterion) {
    let mut group = c.benchmark_group("field_access/mono_first");
    let type_decl = "type R = { x :: Int, y :: Int, z :: Int, w :: Int }";
    let ctor = "{ x: 1, y: 2, z: 3, w: 4 }";
    for n in [100usize, 1_000, 5_000] {
        let src = make_flat_get_field("x", type_decl, ctor, n);
        let prog = compile(&src);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(vm.call("bench", vec![]).unwrap());
            })
        });
    }
    group.finish();
}

fn bench_mono_last(c: &mut Criterion) {
    let mut group = c.benchmark_group("field_access/mono_last");
    let type_decl = "type R = { a :: Int, b :: Int, c :: Int, d :: Int, e :: Int, f :: Int, g :: Int, h :: Int }";
    let ctor = "{ a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8 }";
    for n in [100usize, 1_000, 5_000] {
        let src = make_flat_get_field("h", type_decl, ctor, n);
        let prog = compile(&src);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(vm.call("bench", vec![]).unwrap());
            })
        });
    }
    group.finish();
}

const CHAIN_SRC: &str = r#"
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

fn bench_mono_chain(c: &mut Criterion) {
    let prog = compile(CHAIN_SRC);
    let mut group = c.benchmark_group("field_access/mono_chain");
    for n in [100i64, 1_000, 5_000] {
        group.throughput(Throughput::Elements((n * 3) as u64)); // 3 GetField per iter
        group.bench_function(format!("n={n}"), |b| {
            b.iter(|| {
                let mut vm = Vm::new(&prog);
                vm.set_step_limit(u64::MAX);
                black_box(vm.call("bench", vec![Value::Int(n)]).unwrap());
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_mono_first, bench_mono_last, bench_mono_chain);
criterion_main!(benches);
