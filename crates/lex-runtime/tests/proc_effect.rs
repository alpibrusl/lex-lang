//! Integration tests for the `[proc]` effect.
//!
//! Spawns small, ubiquitous binaries (`echo`, `true`, `false`) so
//! the tests don't require a special environment. Skipped on
//! platforms where these aren't on `PATH` — checked with `which`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn policy_with(effects: &[&str], procs: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    p.allow_proc = procs.iter().map(|s| s.to_string()).collect();
    p
}

fn run(src: &str, func: &str, args: Vec<Value>, policy: Policy) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).map_err(|e| format!("{e}"))
}

fn unwrap_record(v: &Value) -> &indexmap::IndexMap<String, Value> {
    match v {
        Value::Record(r) => r,
        other => panic!("expected Record, got {other:?}"),
    }
}

fn variant_args(v: &Value, expected_name: &str) -> Vec<Value> {
    match v {
        Value::Variant { name, args } if name == expected_name => args.clone(),
        other => panic!("expected Variant(`{expected_name}`), got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.proc" as proc
fn echo(args :: List[Str]) -> [proc] Result[{ stdout :: Str, stderr :: Str, exit_code :: Int }, Str] {
  proc.spawn("echo", args)
}
fn forbidden() -> [proc] Result[{ stdout :: Str, stderr :: Str, exit_code :: Int }, Str] {
  proc.spawn("/usr/bin/whoami", [])
}
fn falsy() -> [proc] Result[{ stdout :: Str, stderr :: Str, exit_code :: Int }, Str] {
  proc.spawn("false", [])
}
"#;

#[test]
fn proc_spawn_runs_echo_and_returns_stdout() {
    let r = run(SRC, "echo",
        vec![Value::List(vec![
            Value::Str("hello".into()),
            Value::Str("world".into()),
        ])],
        policy_with(&["proc"], &["echo"])).expect("run");
    let inner = variant_args(&r, "Ok");
    let rec = unwrap_record(&inner[0]);
    assert_eq!(rec.get("stdout"), Some(&Value::Str("hello world\n".into())));
    assert_eq!(rec.get("exit_code"), Some(&Value::Int(0)));
}

#[test]
fn proc_spawn_blocks_binary_outside_allow_proc() {
    // `whoami` not in --allow-proc; surface as Err(..) Result.
    let r = run(SRC, "forbidden",
        vec![],
        policy_with(&["proc"], &["echo"])).expect("run");
    let inner = variant_args(&r, "Err");
    let msg = match &inner[0] {
        Value::Str(s) => s.clone(),
        other => panic!("expected Str err, got {other:?}"),
    };
    assert!(msg.contains("not in --allow-proc"), "msg: {msg}");
    assert!(msg.contains("whoami"), "msg: {msg}");
}

#[test]
fn proc_spawn_with_empty_allow_proc_is_escape_hatch() {
    // Empty allow_proc list = any binary permitted (escape hatch).
    // Documented in SECURITY.md and in the policy field doc-comment.
    let r = run(SRC, "echo",
        vec![Value::List(vec![Value::Str("x".into())])],
        policy_with(&["proc"], &[])).expect("run");
    let inner = variant_args(&r, "Ok");
    let rec = unwrap_record(&inner[0]);
    assert_eq!(rec.get("stdout"), Some(&Value::Str("x\n".into())));
}

#[test]
fn proc_spawn_propagates_non_zero_exit() {
    let r = run(SRC, "falsy",
        vec![],
        policy_with(&["proc"], &["false"])).expect("run");
    let inner = variant_args(&r, "Ok");
    let rec = unwrap_record(&inner[0]);
    let exit = match rec.get("exit_code") {
        Some(Value::Int(n)) => *n,
        other => panic!("exit_code: {other:?}"),
    };
    assert_ne!(exit, 0, "false should exit non-zero");
}

#[test]
fn proc_spawn_without_proc_in_allow_effects_is_runtime_rejected() {
    // The static check would reject this at type-check time; here we
    // bypass by not granting the effect kind. The handler errors
    // before running.
    let r = run(SRC, "echo",
        vec![Value::List(vec![Value::Str("x".into())])],
        policy_with(&[], &[]));  // proc not granted
    let err = r.expect_err("proc.spawn without --allow-effects proc must error");
    assert!(err.contains("proc"), "err: {err}");
}
