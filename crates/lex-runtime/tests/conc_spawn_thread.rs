//! End-to-end tests for #623: `conc.spawn_thread`.
//!
//! Unlike the synchronous actor ops (`conc.spawn`/`ask`/`tell`, #381),
//! `spawn_thread` runs a zero-arg closure on a fresh *detached* OS
//! thread and returns `Unit` immediately. These tests cover:
//!  1. the closure actually runs on a background thread, and
//!     `spawn_thread` returns before it finishes (detached), and
//!  2. the closure's effect row propagates into the call row
//!     (`[concurrent, Eff]`), so under-declaring the caller is a
//!     type error.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn type_check(src: &str) -> Result<(), Vec<lex_types::TypeError>> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).map(|_| ())
}

// ── runtime: the closure runs on a detached background thread ────────────────

#[test]
fn spawn_thread_runs_closure_detached() {
    // Unique sentinel path so parallel test binaries don't collide.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sentinel = std::env::temp_dir().join(format!(
        "lex_spawn_thread_{}_{nanos}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&sentinel);
    let path = sentinel.to_string_lossy().replace('\\', "/");

    // The worker sleeps before writing, so if `spawn_thread` were
    // synchronous the sentinel would exist by the time `main` returns.
    let src = format!(
        r#"
import "std.conc" as conc
import "std.io"   as io
import "std.time" as time

fn worker() -> [io, time] Unit {{
  let _ := time.sleep_ms(150)
  let _ := io.write("{path}", "ok")
  ()
}}

fn main() -> [concurrent, io, time] Bool {{
  let _ := conc.spawn_thread(worker)
  true
}}
"#
    );

    let prog = parse_source(&src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("type-check");
    let bc = Arc::new(compile_program(&stages));
    let policy = Policy::permissive();
    // `spawn_thread` needs a Program reference to build a per-thread VM.
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(bc.as_ref(), Box::new(handler));

    let ret = vm.call("main", vec![]).expect("vm");
    assert_eq!(ret, Value::Bool(true), "main should return immediately");

    // Detached: the worker sleeps 150ms first, so nothing is written yet.
    assert!(
        !sentinel.exists(),
        "closure ran synchronously — spawn_thread did not detach"
    );

    // Poll up to ~5s for the background thread to do its write.
    let mut wrote = false;
    for _ in 0..500 {
        if let Ok(s) = std::fs::read_to_string(&sentinel) {
            if s == "ok" {
                wrote = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = std::fs::remove_file(&sentinel);
    assert!(wrote, "background thread never ran the closure");
}

// ── types: the closure's effects propagate into the call row ────────────────

#[test]
fn spawn_thread_propagates_closure_effects() {
    // worker is [io, time]; the call row is [concurrent, io, time],
    // which main declares in full → type-checks.
    let src = r#"
import "std.conc" as conc
import "std.io"   as io
import "std.time" as time

fn worker() -> [io, time] Unit {
  let _ := time.sleep_ms(1)
  let _ := io.write("/tmp/x", "y")
  ()
}

fn main() -> [concurrent, io, time] Unit {
  conc.spawn_thread(worker)
}
"#;
    type_check(src).expect("type-check should accept the propagated effects");
}

#[test]
fn spawn_thread_under_declared_effects_is_type_error() {
    // Same worker, but main declares only [concurrent] — the propagated
    // [io, time] leak, so the caller's signature is under-declared.
    let src = r#"
import "std.conc" as conc
import "std.io"   as io
import "std.time" as time

fn worker() -> [io, time] Unit {
  let _ := time.sleep_ms(1)
  let _ := io.write("/tmp/x", "y")
  ()
}

fn main() -> [concurrent] Unit {
  conc.spawn_thread(worker)
}
"#;
    assert!(
        type_check(src).is_err(),
        "under-declared caller should be a type error (effects must propagate)"
    );
}
