//! Tests for `str.slice` clamping behaviour.
//!
//! Until 2026-05-08 `str.slice` errored on any out-of-range
//! `hi` (or any negative `lo`). That's correct as a type-system
//! guarantee but inconvenient when slicing fixed-size prefixes off
//! data of unknown length — e.g. taking the first 64 chars of a
//! license header that may itself be only 32 chars. A real Rubric
//! port tripped on this against a 32-char LICENSE file.
//!
//! New behaviour: `hi` is clamped to `s.len()`, `lo` is clamped to
//! `[0, s.len()]`. `lo > hi` after clamping still errors (caller
//! logic bug). A mid-codepoint `lo` after clamping still errors so
//! silent UTF-8 truncation never sneaks through.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return Err(format!("type errors: {errs:#?}"));
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).map_err(|e| format!("{e}"))
}

const SRC: &str = r#"
import "std.str" as str
fn slice(s :: Str, lo :: Int, hi :: Int) -> Str { str.slice(s, lo, hi) }
"#;

#[test]
fn in_range_slice_returns_substring() {
    let v = run(SRC, "slice", vec![
        Value::Str("hello".into()), Value::Int(1), Value::Int(4),
    ]).unwrap();
    assert_eq!(v, Value::Str("ell".into()));
}

#[test]
fn hi_past_end_clamps_to_len() {
    // The 32-char-LICENSE case — agent code did `slice(text, 0, 64)`
    // and was rejected. Now `hi` clamps to `len`.
    let v = run(SRC, "slice", vec![
        Value::Str("MIT License (32 char-ish here)".into()),
        Value::Int(0), Value::Int(64),
    ]).unwrap();
    assert_eq!(v, Value::Str("MIT License (32 char-ish here)".into()));
}

#[test]
fn lo_past_end_yields_empty_string() {
    // `slice(s, 100, 200)` on a 5-char string clamps both ends to 5
    // and returns "". Cleaner than erroring — a downstream
    // length-check still works.
    let v = run(SRC, "slice", vec![
        Value::Str("hello".into()),
        Value::Int(100), Value::Int(200),
    ]).unwrap();
    assert_eq!(v, Value::Str("".into()));
}

#[test]
fn negative_lo_clamps_to_zero() {
    // Casual `slice(s, -5, 3)` (e.g. agent emitting "the last N
    // characters" before realising slices are absolute) clamps
    // `lo` to 0 instead of erroring.
    let v = run(SRC, "slice", vec![
        Value::Str("hello".into()),
        Value::Int(-5), Value::Int(3),
    ]).unwrap();
    assert_eq!(v, Value::Str("hel".into()));
}

#[test]
fn negative_hi_clamps_to_zero_yielding_empty() {
    let v = run(SRC, "slice", vec![
        Value::Str("hello".into()),
        Value::Int(0), Value::Int(-1),
    ]).unwrap();
    assert_eq!(v, Value::Str("".into()));
}

#[test]
fn lo_greater_than_hi_after_clamping_errors() {
    // Caller logic bug: still surfaces an error so it's not silently
    // ignored. The error mentions "reversed" so messages don't get
    // confused with the old "out of range" wording.
    let err = run(SRC, "slice", vec![
        Value::Str("hello".into()),
        Value::Int(4), Value::Int(2),
    ]).unwrap_err();
    assert!(err.to_lowercase().contains("reversed"),
        "expected reversed-range error, got: {err}");
}

#[test]
fn mid_codepoint_lo_still_errors() {
    // "héllo" is 6 bytes (h-é(0xc3 0xa9)-l-l-o). lo=2 lands inside
    // 'é'. Clamping is purely length-based; UTF-8 boundary errors
    // remain so callers don't accidentally produce ill-formed
    // strings.
    let err = run(SRC, "slice", vec![
        Value::Str("héllo".into()),
        Value::Int(2), Value::Int(5),
    ]).unwrap_err();
    assert!(err.contains("char boundaries"),
        "expected char-boundary error, got: {err}");
}

#[test]
fn empty_string_with_any_range_is_empty() {
    let v = run(SRC, "slice", vec![
        Value::Str("".into()), Value::Int(0), Value::Int(10),
    ]).unwrap();
    assert_eq!(v, Value::Str("".into()));
}
