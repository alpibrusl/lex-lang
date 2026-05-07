//! Integration tests for `std.math` scalar Float ops added in the
//! agents-only stdlib batch (#218).
//!
//! Exercises the new trig, transcendental, rounding, and 2-arg ops to
//! prove the type-checker signature and runtime dispatch agree. The
//! existing matrix ops are covered by `ml_app.rs`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn f(v: Value) -> f64 {
    match v {
        Value::Float(x) => x,
        other => panic!("expected Float, got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.math" as math

fn t_sin(x :: Float)   -> Float { math.sin(x) }
fn t_cos(x :: Float)   -> Float { math.cos(x) }
fn t_tan(x :: Float)   -> Float { math.tan(x) }
fn t_asin(x :: Float)  -> Float { math.asin(x) }
fn t_acos(x :: Float)  -> Float { math.acos(x) }
fn t_atan(x :: Float)  -> Float { math.atan(x) }
fn t_atan2(y :: Float, x :: Float) -> Float { math.atan2(y, x) }
fn t_log2(x :: Float)  -> Float { math.log2(x) }
fn t_log10(x :: Float) -> Float { math.log10(x) }
fn t_floor(x :: Float) -> Float { math.floor(x) }
fn t_ceil(x :: Float)  -> Float { math.ceil(x) }
fn t_round(x :: Float) -> Float { math.round(x) }
fn t_trunc(x :: Float) -> Float { math.trunc(x) }
fn t_pow(a :: Float, b :: Float) -> Float { math.pow(a, b) }
fn t_min(a :: Float, b :: Float) -> Float { math.min(a, b) }
fn t_max(a :: Float, b :: Float) -> Float { math.max(a, b) }
"#;

const EPS: f64 = 1e-12;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < EPS
}

#[test]
fn sin_zero_is_zero() {
    assert!(close(f(run(SRC, "t_sin", vec![Value::Float(0.0)])), 0.0));
}

#[test]
fn cos_zero_is_one() {
    assert!(close(f(run(SRC, "t_cos", vec![Value::Float(0.0)])), 1.0));
}

#[test]
fn sin_squared_plus_cos_squared() {
    let x = 0.7;
    let s = f(run(SRC, "t_sin", vec![Value::Float(x)]));
    let c = f(run(SRC, "t_cos", vec![Value::Float(x)]));
    assert!(close(s * s + c * c, 1.0));
}

#[test]
fn tan_round_trips_through_atan() {
    let x = 0.4;
    let t = f(run(SRC, "t_tan", vec![Value::Float(x)]));
    let back = f(run(SRC, "t_atan", vec![Value::Float(t)]));
    assert!(close(back, x));
}

#[test]
fn asin_acos_inverses() {
    let x = 0.3;
    let a = f(run(SRC, "t_asin", vec![Value::Float(x)]));
    assert!(close(a.sin(), x));
    let c = f(run(SRC, "t_acos", vec![Value::Float(x)]));
    assert!(close(c.cos(), x));
}

#[test]
fn atan2_quadrants() {
    let q1 = f(run(SRC, "t_atan2", vec![Value::Float(1.0), Value::Float(1.0)]));
    assert!(close(q1, std::f64::consts::FRAC_PI_4));
    let q2 = f(run(SRC, "t_atan2", vec![Value::Float(1.0), Value::Float(-1.0)]));
    assert!(close(q2, 3.0 * std::f64::consts::FRAC_PI_4));
}

#[test]
fn log2_powers_of_two() {
    assert!(close(f(run(SRC, "t_log2", vec![Value::Float(8.0)])), 3.0));
    assert!(close(f(run(SRC, "t_log2", vec![Value::Float(1024.0)])), 10.0));
}

#[test]
fn log10_powers_of_ten() {
    assert!(close(f(run(SRC, "t_log10", vec![Value::Float(1000.0)])), 3.0));
}

#[test]
fn floor_ceil_round_trunc() {
    let x = 2.5;
    assert!(close(f(run(SRC, "t_floor", vec![Value::Float(x)])), 2.0));
    assert!(close(f(run(SRC, "t_ceil",  vec![Value::Float(x)])), 3.0));
    assert!(close(f(run(SRC, "t_round", vec![Value::Float(x)])), 3.0));
    assert!(close(f(run(SRC, "t_trunc", vec![Value::Float(x)])), 2.0));

    let n = -2.5;
    assert!(close(f(run(SRC, "t_floor", vec![Value::Float(n)])), -3.0));
    assert!(close(f(run(SRC, "t_ceil",  vec![Value::Float(n)])), -2.0));
    assert!(close(f(run(SRC, "t_trunc", vec![Value::Float(n)])), -2.0));
}

#[test]
fn pow_basics() {
    assert!(close(f(run(SRC, "t_pow", vec![Value::Float(2.0), Value::Float(10.0)])), 1024.0));
    assert!(close(f(run(SRC, "t_pow", vec![Value::Float(9.0), Value::Float(0.5)])),  3.0));
}

#[test]
fn min_max_basics() {
    assert!(close(f(run(SRC, "t_min", vec![Value::Float(1.0), Value::Float(2.0)])), 1.0));
    assert!(close(f(run(SRC, "t_max", vec![Value::Float(1.0), Value::Float(2.0)])), 2.0));
}
