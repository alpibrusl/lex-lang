//! Spec-checker as a runtime action gate (#186).
//!
//! `evaluate_gate` evaluates one or more specs against
//! caller-supplied bindings (typically `state` + the proposed
//! action) and returns `Allow` if every spec holds, `Deny` on
//! the first violation.
//!
//! This is a separate evaluation mode from
//! [`crate::check_spec`]: the latter quantifies over random
//! inputs to discover counterexamples; the former takes the
//! inputs *given* and answers a single deterministic verdict.
//! Specs reuse their existing AST — quantifier names become the
//! lookup keys for the supplied bindings — so the same spec
//! that an offline checker proves over random inputs can also
//! gate one specific action online.
//!
//! Trace integration is intentionally not wired here — the
//! function returns the verdict, and the caller (e.g. an agent
//! runtime in a downstream crate) records it. Keeps spec-checker
//! free of a lex-trace dependency.

use crate::ast::{Spec, SpecExpr, SpecOp};
use indexmap::IndexMap;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use serde::{Deserialize, Serialize};

/// Verdict returned by [`evaluate_gate`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum GateVerdict {
    Allow,
    /// Spec returned `false` (action violates an invariant).
    /// `spec_name` is the name of the offending spec, `reason`
    /// is human-readable detail (typically the spec name plus
    /// the relevant bindings).
    Deny { spec_name: String, reason: String },
    /// Evaluation failed for a non-spec reason (e.g. the body
    /// referenced a Lex function whose call errored). Surfaced
    /// as a separate variant so callers can distinguish "spec
    /// said no" from "we couldn't tell."
    Inconclusive { spec_name: String, reason: String },
}

/// Evaluate every `spec` against `bindings` and return the
/// first non-Allow verdict (or `Allow` if all pass). `lex_source`
/// supplies the host program — any `SpecExpr::Call` in a spec's
/// body resolves to a function in this program.
///
/// Designed for synchronous per-action use. The Lex program is
/// type-checked and compiled on each call; callers that gate at
/// high frequency should prefer [`evaluate_gate_compiled`].
pub fn evaluate_gate(
    specs: &[Spec],
    bindings: &IndexMap<String, Value>,
    lex_source: &str,
) -> GateVerdict {
    let prog = match parse_source(lex_source) {
        Ok(p) => p,
        Err(e) => return GateVerdict::Inconclusive {
            spec_name: "<parse>".into(),
            reason: format!("parse: {e}"),
        },
    };
    let stages = lex_ast::canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return GateVerdict::Inconclusive {
            spec_name: "<typecheck>".into(),
            reason: format!("typecheck: {errs:?}"),
        };
    }
    let bc = compile_program(&stages);
    evaluate_gate_compiled(specs, bindings, &bc)
}

/// Same as [`evaluate_gate`] but takes already-compiled
/// bytecode. Use when gating at high frequency: compile the
/// program once, evaluate many actions against it.
pub fn evaluate_gate_compiled(
    specs: &[Spec],
    bindings: &IndexMap<String, Value>,
    bc: &lex_bytecode::Program,
) -> GateVerdict {
    let policy = Policy::permissive();
    for spec in specs {
        match eval_body(&spec.body, bindings, bc, &policy) {
            Ok(Value::Bool(true)) => continue,
            Ok(Value::Bool(false)) => {
                return GateVerdict::Deny {
                    spec_name: spec.name.clone(),
                    reason: format!(
                        "spec `{}` returned false; bindings: {}",
                        spec.name,
                        format_bindings(bindings),
                    ),
                };
            }
            Ok(other) => return GateVerdict::Inconclusive {
                spec_name: spec.name.clone(),
                reason: format!("spec body returned non-bool: {other:?}"),
            },
            Err(e) => return GateVerdict::Inconclusive {
                spec_name: spec.name.clone(),
                reason: e,
            },
        }
    }
    GateVerdict::Allow
}

fn format_bindings(b: &IndexMap<String, Value>) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(b.len());
    for (k, v) in b {
        parts.push(format!("{k}={}", short_value(v)));
    }
    parts.join(", ")
}

fn short_value(v: &Value) -> String {
    match v {
        Value::Int(i) => format!("{i}"),
        Value::Float(f) => format!("{f}"),
        Value::Bool(b) => format!("{b}"),
        Value::Str(s) => format!("\"{}\"", s.chars().take(40).collect::<String>()),
        other => format!("{other:?}"),
    }
}

/// Evaluate a `SpecExpr` against caller-supplied `bindings`.
/// Mirrors `checker::eval` but kept separate so the gate path
/// doesn't have to thread random-generation state.
fn eval_body(
    e: &SpecExpr,
    bindings: &IndexMap<String, Value>,
    bc: &lex_bytecode::Program,
    policy: &Policy,
) -> Result<Value, String> {
    match e {
        SpecExpr::IntLit { value } => Ok(Value::Int(*value)),
        SpecExpr::FloatLit { value } => Ok(Value::Float(*value)),
        SpecExpr::BoolLit { value } => Ok(Value::Bool(*value)),
        SpecExpr::StrLit { value } => Ok(Value::Str(value.clone())),
        SpecExpr::Var { name } => bindings.get(name).cloned()
            .ok_or_else(|| format!("unbound spec var `{name}` (provide via gate bindings)")),
        SpecExpr::Let { name, value, body } => {
            let v = eval_body(value, bindings, bc, policy)?;
            let mut next = bindings.clone();
            next.insert(name.clone(), v);
            eval_body(body, &next, bc, policy)
        }
        SpecExpr::Not { expr } => match eval_body(expr, bindings, bc, policy)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => Err(format!("not on non-bool: {other:?}")),
        },
        SpecExpr::BinOp { op, lhs, rhs } => {
            let a = eval_body(lhs, bindings, bc, policy)?;
            let b = eval_body(rhs, bindings, bc, policy)?;
            apply_binop(*op, a, b)
        }
        SpecExpr::Call { func, args } => {
            let mut argv = Vec::new();
            for a in args { argv.push(eval_body(a, bindings, bc, policy)?); }
            let handler = DefaultHandler::new(policy.clone());
            let mut vm = Vm::with_handler(bc, Box::new(handler));
            vm.call(func, argv).map_err(|e| format!("call `{func}`: {e}"))
        }
    }
}

fn apply_binop(op: SpecOp, a: Value, b: Value) -> Result<Value, String> {
    use SpecOp::*;
    match (op, &a, &b) {
        (Add, Value::Int(x), Value::Int(y)) => Ok(Value::Int(x + y)),
        (Sub, Value::Int(x), Value::Int(y)) => Ok(Value::Int(x - y)),
        (Mul, Value::Int(x), Value::Int(y)) => Ok(Value::Int(x * y)),
        (Div, Value::Int(x), Value::Int(y)) if *y != 0 => Ok(Value::Int(x / y)),
        (Mod, Value::Int(x), Value::Int(y)) if *y != 0 => Ok(Value::Int(x % y)),
        (Add, Value::Float(x), Value::Float(y)) => Ok(Value::Float(x + y)),
        (Sub, Value::Float(x), Value::Float(y)) => Ok(Value::Float(x - y)),
        (Mul, Value::Float(x), Value::Float(y)) => Ok(Value::Float(x * y)),
        (Div, Value::Float(x), Value::Float(y)) => Ok(Value::Float(x / y)),
        (Eq, x, y) => Ok(Value::Bool(x == y)),
        (Neq, x, y) => Ok(Value::Bool(x != y)),
        (Lt, Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x < y)),
        (Le, Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x <= y)),
        (Gt, Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x > y)),
        (Ge, Value::Int(x), Value::Int(y)) => Ok(Value::Bool(x >= y)),
        (Lt, Value::Float(x), Value::Float(y)) => Ok(Value::Bool(x < y)),
        (Le, Value::Float(x), Value::Float(y)) => Ok(Value::Bool(x <= y)),
        (Gt, Value::Float(x), Value::Float(y)) => Ok(Value::Bool(x > y)),
        (Ge, Value::Float(x), Value::Float(y)) => Ok(Value::Bool(x >= y)),
        (And, Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(*x && *y)),
        (Or, Value::Bool(x), Value::Bool(y)) => Ok(Value::Bool(*x || *y)),
        _ => Err(format!("invalid binop {op:?} on {a:?}, {b:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_spec;

    fn b<I: IntoIterator<Item = (&'static str, Value)>>(items: I) -> IndexMap<String, Value> {
        items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn allow_when_spec_returns_true() {
        let spec = parse_spec("spec ok { forall x :: Int : x + 1 > x }").unwrap();
        let v = evaluate_gate(&[spec], &b([("x", Value::Int(5))]), "");
        assert_eq!(v, GateVerdict::Allow);
    }

    #[test]
    fn deny_when_spec_returns_false() {
        let spec = parse_spec("spec budget { forall used :: Int, delta :: Int : (used + delta) <= 100 }").unwrap();
        let v = evaluate_gate(
            &[spec],
            &b([("used", Value::Int(80)), ("delta", Value::Int(30))]),
            "",
        );
        match v {
            GateVerdict::Deny { spec_name, reason } => {
                assert_eq!(spec_name, "budget");
                assert!(reason.contains("used=80"), "reason should include bindings: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn first_failing_spec_is_reported() {
        // Two specs, second one fails — verdict mentions the
        // second one specifically.
        let s1 = parse_spec("spec always { forall x :: Int : x == x }").unwrap();
        let s2 = parse_spec("spec never { forall x :: Int : x != x }").unwrap();
        let v = evaluate_gate(&[s1, s2], &b([("x", Value::Int(0))]), "");
        match v {
            GateVerdict::Deny { spec_name, .. } => assert_eq!(spec_name, "never"),
            other => panic!("expected Deny on `never`, got {other:?}"),
        }
    }

    #[test]
    fn missing_binding_is_inconclusive_not_panic() {
        // An action that omits a bound the spec needs — surface
        // as Inconclusive so the caller can fix the gate harness
        // rather than crash.
        let spec = parse_spec("spec needs_x { forall x :: Int : x > 0 }").unwrap();
        let v = evaluate_gate(&[spec], &b([]), "");
        match v {
            GateVerdict::Inconclusive { reason, .. } => {
                assert!(reason.contains("unbound spec var"),
                    "expected unbound-var error, got: {reason}");
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    #[test]
    fn grid_budget_phase1_spec() {
        // Headline soft Phase 1 spec: site grid load (active +
        // scheduled + delta) must not exceed budget.
        let spec = parse_spec(r#"
            spec grid_budget {
              forall active :: Int, scheduled :: Int, delta :: Int, budget :: Int :
                (active + scheduled + delta) <= budget
            }
        "#).unwrap();
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("active", Value::Int(40)),
            ("scheduled", Value::Int(20)),
            ("delta", Value::Int(15)),
            ("budget", Value::Int(100)),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
        let deny = evaluate_gate(&[spec], &b([
            ("active", Value::Int(40)),
            ("scheduled", Value::Int(20)),
            ("delta", Value::Int(60)),
            ("budget", Value::Int(100)),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn soc_reserve_phase1_spec() {
        // Second Phase 1 spec: vehicle projected SoC after
        // proposed action must not drop below reserve.
        let spec = parse_spec(r#"
            spec soc_reserve {
              forall soc :: Int, draw :: Int, reserve :: Int :
                (soc - draw) >= reserve
            }
        "#).unwrap();
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("soc", Value::Int(80)),
            ("draw", Value::Int(20)),
            ("reserve", Value::Int(40)),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
        let deny = evaluate_gate(&[spec], &b([
            ("soc", Value::Int(50)),
            ("draw", Value::Int(20)),
            ("reserve", Value::Int(40)),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn gate_is_fast_enough_for_synchronous_use() {
        // Issue calls for single-digit ms per verdict on Phase 1's
        // small spec set. We measure 1k iterations and assert the
        // average is comfortably under that — the headroom matters
        // because CI runners are slower than local hardware.
        let s1 = parse_spec(r#"
            spec grid_budget {
              forall active :: Int, scheduled :: Int, delta :: Int, budget :: Int :
                (active + scheduled + delta) <= budget
            }
        "#).unwrap();
        let s2 = parse_spec(r#"
            spec soc_reserve {
              forall soc :: Int, draw :: Int, reserve :: Int :
                (soc - draw) >= reserve
            }
        "#).unwrap();
        let bindings = b([
            ("active", Value::Int(40)),
            ("scheduled", Value::Int(20)),
            ("delta", Value::Int(15)),
            ("budget", Value::Int(100)),
            ("soc", Value::Int(80)),
            ("draw", Value::Int(20)),
            ("reserve", Value::Int(40)),
        ]);
        let prog = parse_source("").unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let bc = compile_program(&stages);

        let n = 1000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let v = evaluate_gate_compiled(&[s1.clone(), s2.clone()], &bindings, &bc);
            assert_eq!(v, GateVerdict::Allow);
        }
        let elapsed = start.elapsed();
        let per_call_us = elapsed.as_micros() / n as u128;
        assert!(per_call_us < 5_000,
            "per-gate verdict should be under 5ms; got {per_call_us}μs");
    }
}
