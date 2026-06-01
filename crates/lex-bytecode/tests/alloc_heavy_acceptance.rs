//! #463 slice 2b-i — `alloc_heavy` acceptance gate.
//!
//! Asserts the **deterministic** part of the issue's acceptance:
//! a representative handler-shaped workload, when arena lowering is
//! enabled, routes every per-call response record through the
//! arena slab — zero heap fallbacks under the per-Vm counters.
//! The wall-clock ≥2× bar is a humans-read criterion bench; the
//! counter-driven assertions here keep the floor honest against
//! silent regressions (e.g. the lowering quietly stopping firing).
//!
//! Mirrors `response_build_acceptance.rs`'s structure.

use std::sync::{Arc, Mutex, OnceLock};

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Op, Program, Value};
use lex_syntax::parse_source;

/// Serializes compilation across tests — env-var-driven config is
/// process-global, so parallel cargo-test threads would otherwise
/// see a flag set by one test mid-flight in another's `compile`.
fn compile_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

const SRC: &str = r#"
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
    let _g = compile_lock().lock().unwrap();
    if no_lowering {
        // SAFETY: serialized via compile_lock so the env-var read
        // in compile_program can't race a concurrent set/remove.
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

fn count_record_sites(p: &Program) -> (usize, usize) {
    let handle = &p.functions[p.function_names["handle"] as usize];
    let mut arena = 0;
    let mut heap = 0;
    for op in &handle.code {
        match op {
            Op::AllocArenaRecord { .. } => arena += 1,
            Op::MakeRecord { .. } => heap += 1,
            _ => {}
        }
    }
    (arena, heap)
}

#[test]
fn handler_returned_record_lowers_to_arena() {
    let p = compile_with_env(SRC, false);
    let (arena, heap) = count_record_sites(&p);
    assert_eq!(arena, 1, "expected 1 AllocArenaRecord");
    assert_eq!(heap, 0, "no heap MakeRecord should remain after arena lowering");

    let disabled = compile_with_env(SRC, true);
    let (arena_off, heap_off) = count_record_sites(&disabled);
    assert_eq!(arena_off, 0, "lowering should be fully suppressed");
    assert_eq!(heap_off, 1, "expected 1 MakeRecord (heap baseline)");
}

#[test]
fn drive_routes_every_call_to_arena_no_fallbacks() {
    let p = compile_with_env(SRC, false);
    let mut vm = Vm::new(&p);

    let scope = vm.enter_request_scope();
    let r = vm.invoke(p.function_names["drive"], vec![Value::Int(200)]).unwrap();
    // drive returns the sum of `r.total = i*2` for i = 1..=200 = 2*(200*201/2) = 40200.
    assert_eq!(r, Value::Int(40200));

    // 200 handler calls, 1 arena alloc each — and zero fallbacks
    // (an active scope was held the whole time).
    assert_eq!(vm.arena_record_allocs, 200,
        "expected 200 arena allocs (one per drive iteration)");
    assert_eq!(vm.arena_record_heap_fallbacks, 0,
        "no fallback should fire inside the held scope");

    vm.exit_request_scope(scope);
}

#[test]
fn body_hash_unchanged_by_arena_lowering() {
    // The compiler pass is a performance detail — closure identity
    // (#222) must be bit-identical with and without it.
    let on = compile_with_env(SRC, false);
    let off = compile_with_env(SRC, true);
    let on_handle = &on.functions[on.function_names["handle"] as usize];
    let off_handle = &off.functions[off.function_names["handle"] as usize];
    assert_eq!(on_handle.body_hash, off_handle.body_hash);
    let on_drive = &on.functions[on.function_names["drive"] as usize];
    let off_drive = &off.functions[off.function_names["drive"] as usize];
    assert_eq!(on_drive.body_hash, off_drive.body_hash);
}
