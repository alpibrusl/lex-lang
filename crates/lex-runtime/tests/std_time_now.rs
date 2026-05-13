//! Integration tests for `std.time` extensions in #378: `time.now_ms`,
//! `time.now_str`, `time.mono_ns`. The existing `time.now` and
//! `time.sleep_ms` are covered by their respective issue tests; this
//! file only exercises the three new ops.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// `LEX_TEST_NOW` is process-global state; the test cases that set it
/// must serialize so two parallel tests don't race on the env var.
/// Mirrors the env-lock pattern in `crates/lex-runtime/src/llm.rs`.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn policy_with_time() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["time".to_string()].into_iter().collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, fn_name: &str) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy_with_time());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, vec![]).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

const SRC: &str = r#"
import "std.time" as time

fn ms_now() -> [time] Int { time.now_ms() }
fn str_now() -> [time] Str { time.now_str() }
fn mono_now() -> [time] Int { time.mono_ns() }

# Two reads of the monotonic clock back-to-back. The second one must
# be >= the first.
fn mono_pair() -> [time] (Int, Int) {
  let a := time.mono_ns()
  let b := time.mono_ns()
  (a, b)
}
"#;

#[test]
fn now_ms_returns_positive_unix_millis() {
    // Other tests in this file set LEX_TEST_NOW; serialize against
    // them so this one always sees the real wall clock.
    let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let prior = std::env::var("LEX_TEST_NOW").ok();
    std::env::remove_var("LEX_TEST_NOW");
    let v = run(SRC, "ms_now");
    if let Some(s) = prior { std::env::set_var("LEX_TEST_NOW", s); }
    let ms = match v {
        Value::Int(n) => n,
        other => panic!("expected Int, got {other:?}"),
    };
    // A plausible-millis lower bound: any time after Jan 1 2020 UTC.
    // 2020-01-01 00:00:00 UTC = 1_577_836_800 s = 1_577_836_800_000 ms.
    assert!(
        ms > 1_577_836_800_000,
        "time.now_ms should return a recent Unix-millis value; got {ms}"
    );
}

#[test]
fn now_str_returns_iso8601_utc() {
    let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let prior = std::env::var("LEX_TEST_NOW").ok();
    std::env::remove_var("LEX_TEST_NOW");
    let v = run(SRC, "str_now");
    if let Some(s) = prior { std::env::set_var("LEX_TEST_NOW", s); }
    let s = match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    };
    // RFC 3339 / ISO 8601 in UTC: `YYYY-MM-DDTHH:MM:SS.frac+00:00` or `Z`.
    // chrono::DateTime::to_rfc3339 emits `+00:00` for UTC; either form
    // is acceptable per the spec.
    assert!(
        s.len() >= 20,
        "time.now_str should look like an ISO-8601 string; got {s:?}"
    );
    assert!(
        s.contains('T'),
        "time.now_str output should have a 'T' separating date and time; got {s:?}"
    );
    assert!(
        s.ends_with('Z') || s.ends_with("+00:00"),
        "time.now_str should be in UTC (ending with 'Z' or '+00:00'); got {s:?}"
    );
    // Parses back through chrono to confirm well-formedness.
    let _: chrono::DateTime<chrono::Utc> = s.parse().unwrap_or_else(|e| {
        panic!("time.now_str output {s:?} is not a valid RFC 3339 timestamp: {e}")
    });
}

#[test]
fn mono_ns_is_non_decreasing_across_calls() {
    // Two reads in a row must yield (a, b) with b >= a. We don't
    // require strict monotonicity since two consecutive reads on a
    // very fast machine could fall in the same tick; the contract is
    // "monotonic", not "strictly monotonic".
    let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let v = run(SRC, "mono_pair");
    let (a, b) = match v {
        Value::Tuple(xs) if xs.len() == 2 => {
            let a = match &xs[0] { Value::Int(n) => *n, other => panic!("got {other:?}") };
            let b = match &xs[1] { Value::Int(n) => *n, other => panic!("got {other:?}") };
            (a, b)
        }
        other => panic!("expected Tuple, got {other:?}"),
    };
    assert!(a >= 0, "first mono_ns reading must be non-negative; got {a}");
    assert!(b >= a, "mono_ns must be non-decreasing; got first={a}, second={b}");
}

#[test]
fn now_ms_respects_lex_test_now() {
    // `LEX_TEST_NOW` documents seconds; the runtime lifts it to ms by
    // *1000 so callers can pin both `time.now` and `time.now_ms`
    // without setting two env vars.
    let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let prior = std::env::var("LEX_TEST_NOW").ok();
    // 1700000000 s = Nov 14 2023; we expect exactly that *1000 = ms.
    std::env::set_var("LEX_TEST_NOW", "1700000000");
    let v = run(SRC, "ms_now");
    match prior {
        Some(s) => std::env::set_var("LEX_TEST_NOW", s),
        None => std::env::remove_var("LEX_TEST_NOW"),
    }
    let ms = match v {
        Value::Int(n) => n,
        other => panic!("expected Int, got {other:?}"),
    };
    assert_eq!(
        ms, 1_700_000_000_000,
        "time.now_ms should lift LEX_TEST_NOW seconds to ms by ×1000"
    );
}

#[test]
fn now_str_respects_lex_test_now() {
    // Pin to 2020-01-01 00:00:00 UTC = 1577836800 s; expect the
    // formatted string to reflect that exact instant.
    let _g = env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let prior = std::env::var("LEX_TEST_NOW").ok();
    std::env::set_var("LEX_TEST_NOW", "1577836800");
    let v = run(SRC, "str_now");
    match prior {
        Some(s) => std::env::set_var("LEX_TEST_NOW", s),
        None => std::env::remove_var("LEX_TEST_NOW"),
    }
    let s = match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    };
    assert!(
        s.starts_with("2020-01-01T00:00:00"),
        "expected pinned timestamp to render as 2020-01-01T00:00:00...; got {s:?}"
    );
}
