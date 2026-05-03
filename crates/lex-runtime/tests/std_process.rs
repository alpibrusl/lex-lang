//! Integration tests for `std.process`. Closes #97.
//!
//! Tests use POSIX `printf`, `cat`, and `false` — kept to coreutils
//! that ship with every CI image. Skipped on Windows since the
//! commands aren't there.

#![cfg(unix)]

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn policy_with_proc() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["proc".to_string()].into_iter().collect::<BTreeSet<_>>();
    p
}

fn run_with_policy(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

const SRC: &str = r#"
import "std.process" as process
import "std.list" as list
import "std.map" as map
import "std.option" as option

# Empty opts — no cwd, no env, no stdin.
fn empty_opts() -> { cwd :: Option[Str], env :: Map[Str, Str], stdin :: Option[Bytes] } {
  { cwd: None, env: map.new(), stdin: None }
}

# Run and read all of stdout line-by-line, return the line count.
fn run_and_count_stdout(cmd :: Str, args :: List[Str]) -> [proc] Int {
  match process.spawn(cmd, args, empty_opts()) {
    Ok(h) => count_lines(h, 0),
    Err(_) => 0 - 1,
  }
}

# Recursive line-counter; tail-recursive on TCO-capable runtimes,
# stack-friendly for the small line counts in tests.
fn count_lines(h :: ProcessHandle, acc :: Int) -> [proc] Int {
  match process.read_stdout_line(h) {
    Some(_) => count_lines(h, acc + 1),
    None    => match process.wait(h) {
      _ => acc,
    },
  }
}

# Run and return the first stdout line.
fn first_stdout_line(cmd :: Str, args :: List[Str]) -> [proc] Str {
  match process.spawn(cmd, args, empty_opts()) {
    Ok(h) => match process.read_stdout_line(h) {
      Some(line) => line,
      None       => "<no output>",
    },
    Err(_) => "<spawn failed>",
  }
}

# Use process.run for the blocking convenience case.
fn run_capture_stdout(cmd :: Str, args :: List[Str]) -> [proc] Str {
  match process.run(cmd, args) {
    Ok(o)  => o.stdout,
    Err(_) => "<run failed>",
  }
}

fn run_capture_exit(cmd :: Str, args :: List[Str]) -> [proc] Int {
  match process.run(cmd, args) {
    Ok(o)  => o.exit_code,
    Err(_) => 0 - 1,
  }
}
"#;

fn s(v: Value) -> String {
    match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn streaming_spawn_and_read_lines() {
    // printf "a\nb\nc\n" — three lines via the streaming API.
    let v = run_with_policy(
        SRC,
        "run_and_count_stdout",
        vec![
            Value::Str("printf".into()),
            Value::List(vec![Value::Str("a\nb\nc\n".into())]),
        ],
        policy_with_proc(),
    );
    assert_eq!(v, Value::Int(3));
}

#[test]
fn first_stdout_line_returns_first_line() {
    let v = run_with_policy(
        SRC,
        "first_stdout_line",
        vec![
            Value::Str("printf".into()),
            Value::List(vec![Value::Str("alpha\nbeta\ngamma\n".into())]),
        ],
        policy_with_proc(),
    );
    assert_eq!(s(v), "alpha");
}

#[test]
fn run_capture_returns_full_stdout() {
    let v = run_with_policy(
        SRC,
        "run_capture_stdout",
        vec![
            Value::Str("printf".into()),
            Value::List(vec![Value::Str("hello, world".into())]),
        ],
        policy_with_proc(),
    );
    assert_eq!(s(v), "hello, world");
}

#[test]
fn run_capture_exit_for_failing_command() {
    // `false` always exits 1.
    let v = run_with_policy(
        SRC,
        "run_capture_exit",
        vec![Value::Str("false".into()), Value::List(vec![])],
        policy_with_proc(),
    );
    assert_eq!(v, Value::Int(1));
}

#[test]
fn run_capture_exit_for_succeeding_command() {
    let v = run_with_policy(
        SRC,
        "run_capture_exit",
        vec![Value::Str("true".into()), Value::List(vec![])],
        policy_with_proc(),
    );
    assert_eq!(v, Value::Int(0));
}

#[test]
fn allow_proc_basename_blocks_unlisted() {
    let mut p = policy_with_proc();
    p.allow_proc = ["allowed_command_zzz".to_string()].into_iter().collect();
    let v = run_with_policy(
        SRC,
        "run_capture_stdout",
        vec![Value::Str("printf".into()), Value::List(vec![Value::Str("x".into())])],
        p,
    );
    // The Err case in the Lex match returns "<run failed>".
    assert_eq!(s(v), "<run failed>");
}
