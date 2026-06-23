//! Additive scalar gaps (#681):
//!   - std.int min/max/abs (integer counterparts to the Float-only
//!     std.math min/max/abs)
//!   - std.duration millis/minutes/hours/days (the unit set was just
//!     `seconds`, asymmetric with datetime's duration_* constructors)

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

const SRC: &str = r#"
import "std.int"      as int
import "std.datetime" as datetime
import "std.duration" as duration

fn imin(a :: Int, b :: Int) -> Int { int.min(a, b) }
fn imax(a :: Int, b :: Int) -> Int { int.max(a, b) }
fn iabs(a :: Int) -> Int { int.abs(a) }

# Build a Duration via the datetime constructors, read it back in
# each unit. Two days = 48 hours; ninety seconds = 1 minute.
fn two_days_in_hours() -> Int {
  duration.hours(datetime.duration_days(2))
}
fn ninety_seconds_in_minutes() -> Int {
  duration.minutes(datetime.duration_seconds(90.0))
}
fn ninety_seconds_in_millis() -> Int {
  duration.millis(datetime.duration_seconds(90.0))
}
"#;

fn run(fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

#[test]
fn int_min_max_abs() {
    assert_eq!(run("imin", vec![Value::Int(3), Value::Int(7)]), Value::Int(3));
    assert_eq!(run("imax", vec![Value::Int(3), Value::Int(7)]), Value::Int(7));
    assert_eq!(run("iabs", vec![Value::Int(-5)]), Value::Int(5));
    assert_eq!(run("iabs", vec![Value::Int(5)]), Value::Int(5));
    // No lossy Float round-trip: a value beyond f64's exact-integer range.
    let big = 9_007_199_254_740_993_i64; // 2^53 + 1
    assert_eq!(run("imax", vec![Value::Int(big), Value::Int(0)]), Value::Int(big));
}

#[test]
fn duration_unit_extractors() {
    assert_eq!(run("two_days_in_hours", vec![]), Value::Int(48));
    assert_eq!(run("ninety_seconds_in_minutes", vec![]), Value::Int(1));
    assert_eq!(run("ninety_seconds_in_millis", vec![]), Value::Int(90_000));
}
