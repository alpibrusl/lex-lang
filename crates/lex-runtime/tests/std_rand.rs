//! `std.rand.int_in` is an honest uniform draw under the `[random]`
//! effect (#677) — no longer a deterministic midpoint stub, and gated
//! by the same `[random]` grant as `crypto.random` (not a separate
//! `rand` effect).

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

const SRC: &str = r#"
import "std.rand" as rand
fn draw(lo :: Int, hi :: Int) -> [random] Int { rand.int_in(lo, hi) }
"#;

fn policy_with(effects: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    p
}

fn compile() -> Arc<lex_bytecode::Program> {
    let prog = parse_source(SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    Arc::new(compile_program(&stages))
}

fn draw(bc: &Arc<lex_bytecode::Program>, policy: Policy, lo: i64, hi: i64) -> Result<Value, String> {
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(bc));
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    vm.call("draw", vec![Value::Int(lo), Value::Int(hi)]).map_err(|e| e.to_string())
}

#[test]
fn draws_stay_within_inclusive_range() {
    let bc = compile();
    for _ in 0..200 {
        match draw(&bc, policy_with(&["random"]), 3, 7).expect("draw") {
            Value::Int(n) => assert!((3..=7).contains(&n), "draw {n} out of [3, 7]"),
            other => panic!("expected Int, got {other:?}"),
        }
    }
}

#[test]
fn single_point_range_is_exact() {
    let bc = compile();
    assert_eq!(draw(&bc, policy_with(&["random"]), 42, 42).expect("draw"), Value::Int(42));
}

#[test]
fn draws_are_not_constant() {
    // The old stub always returned the midpoint; an honest RNG must
    // produce more than one distinct value over a wide range.
    let bc = compile();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..100 {
        if let Value::Int(n) = draw(&bc, policy_with(&["random"]), 0, 1_000_000).expect("draw") {
            seen.insert(n);
        }
    }
    assert!(seen.len() > 1, "rand.int_in looks constant: {seen:?}");
}

#[test]
fn requires_random_effect_grant() {
    // Gated under [random]; an empty policy must refuse the call.
    let bc = compile();
    let err = draw(&bc, Policy::pure(), 0, 10).expect_err("should be denied without [random]");
    assert!(err.contains("random"), "error should mention the random effect: {err}");
}
