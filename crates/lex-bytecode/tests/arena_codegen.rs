//! #463 slice 2b-i — `apply_arena_lowering` end-to-end tests.
//!
//! Source-level integration tests for the compiler pass that
//! rewrites eligible `MakeRecord` / `MakeTuple` sites to
//! `AllocArenaRecord` / `AllocArenaTuple`. Parallels
//! `stack_records.rs` for #464 step 2.
//!
//! The pass runs **after** `apply_escape_lowering`, so the
//! three-tier story emerges naturally:
//!   - frame-local        → `AllocStackRecord`  (#464, cheapest)
//!   - request-local      → `AllocArenaRecord`  (#463, this slice)
//!   - escapes request    → `MakeRecord`        (heap, status quo)

use std::sync::{Arc, Mutex, OnceLock};

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::{compile_program, Op, Program, Value};
use lex_syntax::parse_source;

/// Serializes compilation across tests in this file. `LEX_NO_ARENA_RECORDS`
/// is process-global; parallel cargo-test threads would otherwise see a
/// var set by `compile_with_no_arena` mid-flight in another test's
/// `compile()` call and the codegen-on / codegen-off bookkeeping would
/// silently mix.
fn compile_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn compile(src: &str) -> Program {
    let _g = compile_lock().lock().unwrap();
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    compile_program(&stages)
}

fn compile_with_no_arena(src: &str) -> Program {
    // SAFETY: serialized via compile_lock() so the env-var read in
    // compile_program can't race a concurrent set/remove from another
    // thread.
    let _g = compile_lock().lock().unwrap();
    unsafe { std::env::set_var("LEX_NO_ARENA_RECORDS", "1"); }
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    let prog = compile_program(&stages);
    unsafe { std::env::remove_var("LEX_NO_ARENA_RECORDS"); }
    prog
}

fn fn_code<'a>(prog: &'a Program, name: &str) -> &'a [Op] {
    let idx = prog.function_names[name];
    &prog.functions[idx as usize].code
}

fn count<F: Fn(&Op) -> bool>(code: &[Op], pred: F) -> usize {
    code.iter().filter(|op| pred(op)).count()
}

// ---------------------------------------------------------------
// Three-tier lowering
// ---------------------------------------------------------------

/// A handler-shaped function that returns a fresh record. The
/// return crosses the frame boundary (so the stack pass leaves it),
/// but the request-scope analysis classifies it as eligible (no
/// `Call`/`EffectCall`/`MakeClosure` hatch on the path to `Return`).
/// The arena pass picks it up.
#[test]
fn returned_record_lowers_to_alloc_arena_record() {
    let src = r#"
        fn handler() -> { status :: Int, total :: Int } {
          { status: 200, total: 42 }
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "handler");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 0,
        "returned record is frame-escaping — not stack: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::AllocArenaRecord { .. })), 1,
        "should lower to arena: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::MakeRecord { .. })), 0,
        "no heap MakeRecord should remain: {code:?}");
}

#[test]
fn returned_tuple_lowers_to_alloc_arena_tuple() {
    let src = r#"
        fn handler() -> Tuple[Int, Int] { (3, 4) }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "handler");
    assert_eq!(count(code, |op| matches!(op, Op::AllocArenaTuple { .. })), 1);
    assert_eq!(count(code, |op| matches!(op, Op::MakeTuple(_))), 0);
}

/// Frame-local record should land on the **stack** tier (cheapest),
/// not the arena tier — confirms the ordering of the two passes.
#[test]
fn frame_local_record_prefers_stack_tier_over_arena() {
    let src = r#"
        fn drop_and_read() -> Int {
          let r := { x: 7, y: 9 }
          r.x
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "drop_and_read");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 1,
        "frame-local record should land on stack tier: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::AllocArenaRecord { .. })), 0,
        "stack pass runs first and takes the cheaper tier: {code:?}");
}

/// A record passed to a call escapes the request scope (intra-
/// procedural conservative). Both passes leave it as `MakeRecord`.
#[test]
fn record_passed_to_call_stays_on_heap_tier() {
    let src = r#"
        fn use_it(r :: { x :: Int, y :: Int }) -> Int { r.x }
        fn caller() -> Int { use_it({ x: 1, y: 2 }) }
    "#;
    let p = compile(src);
    let caller_code = fn_code(&p, "caller");
    assert_eq!(count(caller_code, |op| matches!(op, Op::AllocStackRecord { .. })), 0);
    assert_eq!(count(caller_code, |op| matches!(op, Op::AllocArenaRecord { .. })), 0,
        "Call-passed record escapes the request — not arena: {caller_code:?}");
    assert_eq!(count(caller_code, |op| matches!(op, Op::MakeRecord { .. })), 1,
        "stays on heap tier: {caller_code:?}");
}

// ---------------------------------------------------------------
// Per-site lowering in a mixed function
// ---------------------------------------------------------------

#[test]
fn per_site_lowering_mixes_all_three_tiers() {
    // `temp` is built, read, dropped → stack tier.
    // The returned `{z}` crosses the frame, stays in request → arena tier.
    // The argument to `helper` escapes the request → heap tier.
    let src = r#"
        fn helper(r :: { a :: Int }) -> Int { r.a }
        fn mix() -> { z :: Int } {
          let temp := { a: 1, b: 2 }
          let _ := temp.a
          let _ := helper({ a: 99 })
          { z: 99 }
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "mix");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 1,
        "dropped `temp` should be stack: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::AllocArenaRecord { .. })), 1,
        "returned `{{z}}` should be arena: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::MakeRecord { .. })), 1,
        "helper argument should stay heap: {code:?}");
}

// ---------------------------------------------------------------
// Runtime: arena lowering actually routes through the slab
// ---------------------------------------------------------------

#[test]
fn arena_handler_actually_routes_to_slab_inside_scope() {
    let src = r#"
        fn handler() -> { status :: Int, total :: Int } {
          { status: 200, total: 42 }
        }
    "#;
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let scope = vm.enter_request_scope();
    let result = vm.invoke(p.function_names["handler"], vec![]).unwrap();
    let materialized = vm.materialize_arena_handles(result);
    vm.exit_request_scope(scope);

    // The handler's record routed to arena (1 alloc), no fallback.
    assert_eq!(vm.arena_record_allocs, 1, "should have used arena path");
    assert_eq!(vm.arena_record_heap_fallbacks, 0, "should not have fallen back");

    match materialized {
        Value::Record { fields, .. } => {
            assert_eq!(fields.get("status"), Some(&Value::Int(200)));
            assert_eq!(fields.get("total"), Some(&Value::Int(42)));
        }
        other => panic!("expected materialized Record, got {other:?}"),
    }
}

/// Arena-lowered bytecode invoked outside a scope falls back to
/// `MakeRecord` semantics — the safety net the VM-side handlers
/// provide. Lets arena-lowered code run in REPL / tests / top-level
/// script contexts without crashing.
#[test]
fn arena_handler_outside_scope_falls_back_to_heap() {
    let src = r#"
        fn handler() -> { status :: Int } {
          { status: 200 }
        }
    "#;
    let p = compile(src);
    let mut vm = Vm::new(&p);
    // Deliberately no enter_request_scope.
    let result = vm.invoke(p.function_names["handler"], vec![]).unwrap();
    assert_eq!(vm.arena_record_allocs, 0);
    assert_eq!(vm.arena_record_heap_fallbacks, 1);
    match result {
        Value::Record { fields, .. } => {
            assert_eq!(fields.get("status"), Some(&Value::Int(200)));
        }
        other => panic!("expected heap fallback Record, got {other:?}"),
    }
}

// ---------------------------------------------------------------
// Env var disables the pass + body_hash invariance
// ---------------------------------------------------------------

#[test]
fn lex_no_arena_records_disables_the_pass() {
    let src = r#"
        fn handler() -> { status :: Int } { { status: 200 } }
    "#;
    let on = compile(src);
    let off = compile_with_no_arena(src);

    let on_code = fn_code(&on, "handler");
    let off_code = fn_code(&off, "handler");

    assert_eq!(count(on_code, |op| matches!(op, Op::AllocArenaRecord { .. })), 1,
        "default: pass fires");
    assert_eq!(count(off_code, |op| matches!(op, Op::AllocArenaRecord { .. })), 0,
        "env var: pass disabled");
    assert_eq!(count(off_code, |op| matches!(op, Op::MakeRecord { .. })), 1,
        "env var: record stays heap MakeRecord");
}

#[test]
fn body_hash_unchanged_by_arena_lowering() {
    // The same source compiled with and without arena lowering must
    // produce identical body hashes (closure identity #222 — the
    // lowering is a performance detail invisible to attestation).
    let src = r#"
        fn handler() -> { status :: Int, total :: Int } {
          { status: 200, total: 42 }
        }
    "#;
    let on = Arc::new(compile(src));
    let off = Arc::new(compile_with_no_arena(src));
    let on_fn = &on.functions[on.function_names["handler"] as usize];
    let off_fn = &off.functions[off.function_names["handler"] as usize];
    assert_eq!(on_fn.body_hash, off_fn.body_hash,
        "arena lowering must not perturb body_hash (closure identity #222)");
}
