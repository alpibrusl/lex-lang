//! Integration tests for `std.env` (#216) — runtime env-var access
//! gated by the `[env]` effect.
//!
//! Per-var scoping (`[env(NAME)]`) lands with #207's per-capability
//! effect parameterization; the flat `[env]` is the v1 surface tested
//! here.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

const SRC: &str = r#"
import "std.env" as env

fn lookup(name :: Str) -> [env] Option[Str] {
  env.get(name)
}
"#;

fn policy_with(effects: &[&str]) -> Policy {
    let mut allow = BTreeSet::new();
    for e in effects { allow.insert((*e).to_string()); }
    Policy {
        allow_effects: allow,
        ..Policy::default()
    }
}

fn run(policy: Policy, args: Vec<Value>) -> Value {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call("lookup", args).unwrap_or_else(|e| panic!("call lookup: {e}"))
}

#[test]
fn env_get_returns_some_for_set_var() {
    let key = "LEX_TEST_ENV_KEY_FOR_STD_ENV_TESTS";
    std::env::set_var(key, "the_value");
    let v = run(policy_with(&["env"]), vec![Value::Str(key.into())]);
    match v {
        Value::Variant { name, args } if name == "Some" => {
            match args.into_iter().next() {
                Some(Value::Str(s)) => assert_eq!(s, "the_value"),
                other => panic!("expected Some(Str), got {other:?}"),
            }
        }
        other => panic!("expected Some, got {other:?}"),
    }
    std::env::remove_var(key);
}

#[test]
fn env_get_returns_none_for_unset_var() {
    let key = "LEX_TEST_ENV_KEY_NEVER_SET_xyzzy_42";
    std::env::remove_var(key);
    let v = run(policy_with(&["env"]), vec![Value::Str(key.into())]);
    match v {
        Value::Variant { name, args } if name == "None" => assert!(args.is_empty()),
        other => panic!("expected None, got {other:?}"),
    }
}

#[test]
fn env_get_blocked_without_effect_grant() {
    // Pure policy → `[env]` is not in allow_effects → policy
    // walk rejects the program before it runs.
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("typecheck");
    let bc = Arc::new(compile_program(&stages));
    let report = lex_runtime::check_program(&bc, &Policy::pure());
    assert!(report.is_err(), "expected policy violation, got {:?}", report);
    let violations = report.unwrap_err();
    assert!(
        violations.iter().any(|v| v.effect.as_deref() == Some("env")),
        "expected env effect violation, got {violations:?}"
    );
}
