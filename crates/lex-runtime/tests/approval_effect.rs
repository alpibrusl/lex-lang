//! Integration tests for the `[approval]` effect (human-in-the-loop
//! host boundary, mirroring the `--allow-proc` scope pattern in
//! `proc_effect.rs`).

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{ApprovalSink, DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn policy_with(effects: &[&str], scopes: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    p.allow_approval = scopes.iter().map(|s| s.to_string()).collect();
    p
}

fn run_with(src: &str, func: &str, args: Vec<Value>, handler: DefaultHandler) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).map_err(|e| format!("{e}"))
}

fn run(src: &str, func: &str, args: Vec<Value>, policy: Policy) -> Result<Value, String> {
    run_with(src, func, args, DefaultHandler::new(policy))
}

fn variant_args(v: &Value, expected_name: &str) -> Vec<Value> {
    match v {
        Value::Variant { name, args } if name == expected_name => args.clone(),
        other => panic!("expected Variant(`{expected_name}`), got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.approval" as approval
fn ask(scope :: Str, reason :: Str) -> [approval] Result[Str, Str] {
  approval.request(scope, reason)
}
"#;

struct FixedAnswerSink(&'static str);
impl ApprovalSink for FixedAnswerSink {
    fn request(&self, _scope: &str, _reason: &str) -> Result<String, String> {
        Ok(self.0.to_string())
    }
}

#[test]
fn approval_request_without_sink_is_refused() {
    // Default handler has no configured ApprovalSink — every call is
    // refused rather than silently "approved".
    let r = run(SRC, "ask",
        vec![Value::Str("payment".into()), Value::Str("refund $50".into())],
        policy_with(&["approval"], &[])).expect("run");
    let inner = variant_args(&r, "Err");
    let msg = match &inner[0] {
        Value::Str(s) => s.clone(),
        other => panic!("expected Str err, got {other:?}"),
    };
    assert!(msg.contains("no ApprovalSink configured"), "msg: {msg}");
}

#[test]
fn approval_request_with_sink_returns_operator_answer() {
    let handler = DefaultHandler::new(policy_with(&["approval"], &[]))
        .with_approval_sink(Box::new(FixedAnswerSink("approved")));
    let r = run_with(SRC, "ask",
        vec![Value::Str("payment".into()), Value::Str("refund $50".into())],
        handler).expect("run");
    let inner = variant_args(&r, "Ok");
    assert_eq!(inner[0], Value::Str("approved".into()));
}

#[test]
fn approval_request_blocks_scope_outside_allow_approval() {
    let handler = DefaultHandler::new(policy_with(&["approval"], &["deploy"]))
        .with_approval_sink(Box::new(FixedAnswerSink("approved")));
    let r = run_with(SRC, "ask",
        vec![Value::Str("payment".into()), Value::Str("refund $50".into())],
        handler).expect("run");
    let inner = variant_args(&r, "Err");
    let msg = match &inner[0] {
        Value::Str(s) => s.clone(),
        other => panic!("expected Str err, got {other:?}"),
    };
    assert!(msg.contains("not in --allow-approval"), "msg: {msg}");
    assert!(msg.contains("payment"), "msg: {msg}");
}

#[test]
fn approval_request_with_empty_allow_approval_is_escape_hatch() {
    // Empty allow_approval list = any scope permitted, same wildcard
    // convention as --allow-proc / --allow-net-host.
    let handler = DefaultHandler::new(policy_with(&["approval"], &[]))
        .with_approval_sink(Box::new(FixedAnswerSink("approved")));
    let r = run_with(SRC, "ask",
        vec![Value::Str("anything".into()), Value::Str("reason".into())],
        handler).expect("run");
    let inner = variant_args(&r, "Ok");
    assert_eq!(inner[0], Value::Str("approved".into()));
}

#[test]
fn approval_request_without_effect_in_allow_effects_is_runtime_rejected() {
    let r = run(SRC, "ask",
        vec![Value::Str("payment".into()), Value::Str("reason".into())],
        policy_with(&[], &[]));
    let err = r.expect_err("approval.request without --allow-effects approval must error");
    assert!(err.contains("approval"), "err: {err}");
}
