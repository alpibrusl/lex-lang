//! Named-actor registry (#444). Each test resets the global registry
//! at start so cross-test pollution can't leak. Tests run on a single
//! thread (the registry is a process-wide `Mutex<HashMap>` and parallel
//! tests would observe each other's registrations) — `mod tests` here
//! is fine because each `#[test]` calls `_reset_for_tests` first.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, conc_registry, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::{Mutex, MutexGuard, OnceLock};

// The registry is process-global. Cargo runs tests in this file in
// parallel by default (CI invokes `cargo test --workspace` without
// `--test-threads=1`), so each test serialises through `serial_lock`
// before touching state. `_reset_for_tests` is called *after* taking
// the lock so the slate is clean at the top of every body. The lock
// recovers from a poisoned guard (a prior panic) — we only care about
// mutual exclusion, not about preserving any state across the panic.
fn serial_lock() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm call")
}

/// Quick spawn helper: an actor whose handler accepts an Int message
/// and returns `(state + msg, state + msg)` — state and reply are the
/// running sum. Just enough behaviour to verify the actor's identity
/// is preserved through register / lookup.
const SUM_ACTOR_SRC: &str = r#"
import "std.conc" as conc

fn handler(state :: Int, msg :: Int) -> (Int, Int) {
  let next := state + msg
  (next, next)
}

fn spawn_sum(init :: Int) -> [concurrent] Actor[Int] {
  conc.spawn(init, handler)
}

fn reg(a :: Actor[Int], name :: Str) -> [concurrent] Result[Nil, ConcError] {
  conc.register(a, name)
}

fn unreg(name :: Str) -> [concurrent] Result[Nil, ConcError] {
  conc.unregister(name)
}

fn lk(name :: Str) -> [concurrent] Option[Actor[Int]] {
  conc.lookup(name)
}

fn names() -> [concurrent] List[Str] { conc.registered() }

fn ask_via_lookup(name :: Str, msg :: Int) -> [concurrent] Option[Int] {
  match conc.lookup(name) {
    None    => None,
    Some(a) => Some(conc.ask(a, msg)),
  }
}

fn ask_direct(a :: Actor[Int], msg :: Int) -> [concurrent] Int {
  conc.ask(a, msg)
}
"#;

fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1
            => args.into_iter().next().unwrap(),
        other => panic!("expected Ok(_), got {other:?}"),
    }
}

fn unwrap_err(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Err" && args.len() == 1
            => args.into_iter().next().unwrap(),
        other => panic!("expected Err(_), got {other:?}"),
    }
}

fn unwrap_some(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Some" && args.len() == 1
            => args.into_iter().next().unwrap(),
        other => panic!("expected Some(_), got {other:?}"),
    }
}

#[test]
fn register_first_time_returns_ok() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let actor = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    let r = run(SUM_ACTOR_SRC, "reg", vec![actor, Value::Str("vehicle".into())]);
    assert_eq!(unwrap_ok(r), Value::Unit);

    let names = run(SUM_ACTOR_SRC, "names", vec![]);
    assert_eq!(names, Value::List(vec![Value::Str("vehicle".into())].into()));
}

#[test]
fn register_duplicate_name_returns_already_registered() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let a = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    let b = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(99)]);
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![a, Value::Str("dup".into())]));
    let r2 = run(SUM_ACTOR_SRC, "reg", vec![b, Value::Str("dup".into())]);
    match unwrap_err(r2) {
        Value::Variant { name, args }
            if name == "AlreadyRegistered" && args.len() == 1 =>
        {
            match &args[0] {
                Value::Str(s) => assert_eq!(s.as_str(), "dup"),
                other => panic!("expected Str name, got {other:?}"),
            }
        }
        other => panic!("expected AlreadyRegistered, got {other:?}"),
    }
}

#[test]
fn lookup_unregistered_returns_none() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let r = run(SUM_ACTOR_SRC, "lk", vec![Value::Str("nope".into())]);
    assert_eq!(r, Value::Variant { name: "None".into(), args: vec![] });
}

#[test]
fn lookup_after_register_returns_same_actor_identity() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let actor = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(10)]);
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![actor.clone(), Value::Str("counter".into())]));
    let looked_up = unwrap_some(
        run(SUM_ACTOR_SRC, "lk", vec![Value::Str("counter".into())]));
    // Actor identity equality (Arc::ptr_eq under the hood) — same cell.
    assert_eq!(looked_up, actor);
}

#[test]
fn ask_via_lookup_drives_actor_state() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let a = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![a, Value::Str("acc".into())]));

    let r1 = unwrap_some(run(SUM_ACTOR_SRC, "ask_via_lookup",
        vec![Value::Str("acc".into()), Value::Int(5)]));
    assert_eq!(r1, Value::Int(5));
    let r2 = unwrap_some(run(SUM_ACTOR_SRC, "ask_via_lookup",
        vec![Value::Str("acc".into()), Value::Int(7)]));
    assert_eq!(r2, Value::Int(12), "second ask sees state from the first");
}

#[test]
fn unregister_removes_name_but_existing_handles_still_work() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let a = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![a.clone(), Value::Str("temp".into())]));
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "unreg",
        vec![Value::Str("temp".into())]));

    // Name no longer resolves.
    let r = run(SUM_ACTOR_SRC, "lk", vec![Value::Str("temp".into())]);
    assert_eq!(r, Value::Variant { name: "None".into(), args: vec![] });

    // Direct handle still works — actor cell stays alive while `a` holds it.
    // ask_direct returns the new state (init 0 + msg 3 = 3).
    let reply = run(SUM_ACTOR_SRC, "ask_direct", vec![a, Value::Int(3)]);
    assert_eq!(reply, Value::Int(3),
        "unregistered actor's held handle should still accept messages");
}

#[test]
fn unregister_missing_name_returns_not_registered() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let r = run(SUM_ACTOR_SRC, "unreg", vec![Value::Str("missing".into())]);
    match unwrap_err(r) {
        Value::Variant { name, args }
            if name == "NotRegistered" && args.len() == 1 =>
        {
            match &args[0] {
                Value::Str(s) => assert_eq!(s.as_str(), "missing"),
                other => panic!("{other:?}"),
            }
        }
        other => panic!("expected NotRegistered, got {other:?}"),
    }
}

#[test]
fn registered_lists_names_sorted() {
    let _guard = serial_lock();
    conc_registry::_reset_for_tests();

    let a = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    let b = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    let c = run(SUM_ACTOR_SRC, "spawn_sum", vec![Value::Int(0)]);
    // Register in non-sorted order — output should still be sorted.
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![a, Value::Str("charlie".into())]));
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![b, Value::Str("alpha".into())]));
    let _ = unwrap_ok(run(SUM_ACTOR_SRC, "reg",
        vec![c, Value::Str("bravo".into())]));

    let names = run(SUM_ACTOR_SRC, "names", vec![]);
    assert_eq!(
        names,
        Value::List(vec![
            Value::Str("alpha".into()),
            Value::Str("bravo".into()),
            Value::Str("charlie".into()),
        ].into()),
    );
}

// ── `time.sleep` smoke tests (#445) ─────────────────────────────────────
//
// Folded into this binary rather than a sibling test file so the
// workspace adds one new test binary instead of two — each new binary
// re-links lex-runtime + arrow + polars and the ubuntu-latest CI box
// is already at its disk ceiling. Lower-bound assertions only on
// elapsed time; CI hosts stall and we don't want a flaky upper bound.

const SLEEP_SRC: &str = r#"
import "std.time" as time
import "std.datetime" as dt

fn sleep_for(seconds :: Float) -> [time] Nil {
  time.sleep(dt.duration_seconds(seconds))
}
"#;

#[test]
fn sleep_zero_duration_is_a_noop() {
    let start = std::time::Instant::now();
    let _ = run(SLEEP_SRC, "sleep_for", vec![Value::Float(0.0)]);
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50,
        "zero-duration sleep should return immediately, took {elapsed:?}");
}

#[test]
fn sleep_50ms_elapses_real_wall_time() {
    let start = std::time::Instant::now();
    let _ = run(SLEEP_SRC, "sleep_for", vec![Value::Float(0.05)]);
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() >= 40,
        "50ms sleep should take ~50ms, took {elapsed:?}");
}

#[test]
fn sleep_negative_duration_is_a_noop() {
    let start = std::time::Instant::now();
    let _ = run(SLEEP_SRC, "sleep_for", vec![Value::Float(-1.0)]);
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50,
        "negative-duration sleep should return immediately, took {elapsed:?}");
}
