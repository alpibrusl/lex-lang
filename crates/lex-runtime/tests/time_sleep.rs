//! `time.sleep` smoke tests (#445). Lower-bound assertions only on
//! elapsed wall time — CI hosts stall, so a tight upper bound would be
//! flaky. The three cases (zero, positive, negative) cover the three
//! branches in the runtime: short-circuit on `<= 0`, real sleep
//! otherwise.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

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

const SRC: &str = r#"
import "std.time" as time
import "std.datetime" as dt

fn sleep_for(seconds :: Float) -> [time] Nil {
  time.sleep(dt.duration_seconds(seconds))
}
"#;

#[test]
fn sleep_zero_duration_is_a_noop() {
    let start = std::time::Instant::now();
    let _ = run(SRC, "sleep_for", vec![Value::Float(0.0)]);
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50,
        "zero-duration sleep should return immediately, took {elapsed:?}");
}

#[test]
fn sleep_50ms_elapses_real_wall_time() {
    let start = std::time::Instant::now();
    let _ = run(SRC, "sleep_for", vec![Value::Float(0.05)]);
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() >= 40,
        "50ms sleep should take ~50ms, took {elapsed:?}");
}

#[test]
fn sleep_negative_duration_is_a_noop() {
    let start = std::time::Instant::now();
    let _ = run(SRC, "sleep_for", vec![Value::Float(-1.0)]);
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50,
        "negative-duration sleep should return immediately, took {elapsed:?}");
}
