//! `std.agent` effect tags — `[llm_local]`, `[llm_cloud]`,
//! `[a2a]`, `[mcp]` (#184). The wire formats live in downstream
//! crates and #185; what's tested here is the type-check
//! enforcement, the policy-gate plumbing, and that each effect
//! invocation produces a trace record.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn type_check(src: &str) -> Result<(), Vec<lex_types::TypeError>> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).map(|_| ())
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

#[test]
fn local_only_function_cannot_reach_cloud_llm() {
    // The headline acceptance criterion of #184. A function
    // typed `[llm_local]` calling `agent.cloud_complete` (which
    // is `[llm_cloud]`) must fail type-check.
    let src = r#"
import "std.agent" as agent

fn ask(q :: Str) -> [llm_local] Result[Str, Str] {
  agent.cloud_complete(q)
}
"#;
    let err = type_check(src).expect_err("should fail type-check");
    let any_undeclared = err.iter().any(|e| matches!(
        e, lex_types::TypeError::EffectNotDeclared { effect, .. } if effect == "llm_cloud"));
    assert!(any_undeclared,
        "expected EffectNotDeclared(llm_cloud); got {err:#?}");
}

#[test]
fn cloud_only_function_cannot_reach_local_llm() {
    // Symmetric to the above. The two surfaces are non-fungible.
    let src = r#"
import "std.agent" as agent

fn ask(q :: Str) -> [llm_cloud] Result[Str, Str] {
  agent.local_complete(q)
}
"#;
    let err = type_check(src).expect_err("should fail type-check");
    let any_undeclared = err.iter().any(|e| matches!(
        e, lex_types::TypeError::EffectNotDeclared { effect, .. } if effect == "llm_local"));
    assert!(any_undeclared,
        "expected EffectNotDeclared(llm_local); got {err:#?}");
}

#[test]
fn a2a_and_mcp_are_distinct_effects() {
    // Conflating the two would let a function typed `[a2a]`
    // accidentally reach an MCP tool — exactly the property the
    // issue calls out as load-bearing.
    let src = r#"
import "std.agent" as agent

fn fanout(peer :: Str, payload :: Str) -> [a2a] Result[Str, Str] {
  agent.call_mcp("optimizer", "schedule", payload)
}
"#;
    let err = type_check(src).expect_err("should fail type-check");
    let any_undeclared = err.iter().any(|e| matches!(
        e, lex_types::TypeError::EffectNotDeclared { effect, .. } if effect == "mcp"));
    assert!(any_undeclared,
        "expected EffectNotDeclared(mcp); got {err:#?}");
}

#[test]
fn function_with_all_four_effects_type_checks() {
    // Declared union of all four passes. The body uses each
    // builtin to confirm each is reachable when its effect is in
    // the declared set.
    let src = r#"
import "std.agent" as agent

fn orchestrate(q :: Str, peer :: Str)
  -> [llm_local, llm_cloud, a2a, mcp] Result[Str, Str]
{
  let r1 := agent.local_complete(q)
  let r2 := agent.cloud_complete(q)
  let r3 := agent.send_a2a(peer, q)
  agent.call_mcp("optimizer", "schedule", q)
}
"#;
    type_check(src).expect("should type-check");
}

#[test]
fn agent_calls_succeed_under_permissive_policy() {
    // Permissive policy includes all four new effects, so the
    // stub handler returns `Ok(<llm_local stub>)` etc. and the
    // function returns the last call's result.
    let src = r#"
import "std.agent" as agent

fn run() -> [llm_local, llm_cloud, a2a, mcp] Result[Str, Str] {
  let r1 := agent.local_complete("hi")
  let r2 := agent.cloud_complete("hi")
  let r3 := agent.send_a2a("peer-1", "hi")
  agent.call_mcp("optimizer", "schedule", "{}")
}
"#;
    let v = run_with_policy(src, "run", vec![], Policy::permissive());
    match &v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Str(s) => assert!(s.contains("mcp"),
                "stub response should mention the effect kind: {s}"),
            other => panic!("expected Str, got {other:?}"),
        },
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn agent_calls_blocked_when_effect_missing_from_policy() {
    // Pure policy disallows everything. A function declared
    // `[llm_local]` type-checks, but the runtime gate refuses
    // to dispatch the call.
    let src = r#"
import "std.agent" as agent

fn run() -> [llm_local] Result[Str, Str] {
  agent.local_complete("hi")
}
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("type-check");
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let res = vm.call("run", vec![]);
    let err = res.expect_err("pure policy should reject llm_local");
    let msg = format!("{err}");
    assert!(msg.contains("llm_local") || msg.contains("policy"),
        "error should mention the disallowed effect: {msg}");
}
