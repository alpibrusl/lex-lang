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
    evaluate_gate_compiled_inner(specs, bindings, bc, None)
}

/// Like [`evaluate_gate_compiled`] but additionally threads a
/// caller-supplied tracer into every Vm the spec body spins up
/// for [`SpecExpr::Call`] (#199).
///
/// `new_tracer` is called once per host-helper invocation and
/// must produce a fresh `Box<dyn Tracer>` for each new `Vm`.
/// Multiple tracers can share state — typically by closing over
/// a [`lex_trace::Handle`] and cloning it inside the closure —
/// so the resulting trace tree captures the spec body's call
/// graph (e.g. `under_budget → projected_load + budget_total`)
/// alongside the rest of the agent's run.
///
/// Existing callers of [`evaluate_gate`] / [`evaluate_gate_compiled`]
/// stay unchanged; this is purely additive.
pub fn evaluate_gate_compiled_traced<F>(
    specs: &[Spec],
    bindings: &IndexMap<String, Value>,
    bc: &lex_bytecode::Program,
    new_tracer: F,
) -> GateVerdict
where
    F: Fn() -> Box<dyn lex_bytecode::vm::Tracer>,
{
    evaluate_gate_compiled_inner(specs, bindings, bc, Some(&new_tracer))
}

fn evaluate_gate_compiled_inner(
    specs: &[Spec],
    bindings: &IndexMap<String, Value>,
    bc: &lex_bytecode::Program,
    new_tracer: Option<&dyn Fn() -> Box<dyn lex_bytecode::vm::Tracer>>,
) -> GateVerdict {
    let policy = Policy::permissive();
    for spec in specs {
        match eval_body(&spec.body, bindings, bc, &policy, new_tracer) {
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
///
/// `new_tracer`, when present, is invoked once per
/// `SpecExpr::Call` and the resulting Tracer is attached to
/// the Vm before running the host helper. The factory shape
/// (rather than a single `Box<dyn Tracer>`) is what lets
/// multiple sibling calls all flow into the same caller-side
/// recorder via cloned `Handle`s.
fn eval_body(
    e: &SpecExpr,
    bindings: &IndexMap<String, Value>,
    bc: &lex_bytecode::Program,
    policy: &Policy,
    new_tracer: Option<&dyn Fn() -> Box<dyn lex_bytecode::vm::Tracer>>,
) -> Result<Value, String> {
    match e {
        SpecExpr::IntLit { value } => Ok(Value::Int(*value)),
        SpecExpr::FloatLit { value } => Ok(Value::Float(*value)),
        SpecExpr::BoolLit { value } => Ok(Value::Bool(*value)),
        SpecExpr::StrLit { value } => Ok(Value::Str(value.clone())),
        SpecExpr::Var { name } => bindings.get(name).cloned()
            .ok_or_else(|| format!("unbound spec var `{name}` (provide via gate bindings)")),
        SpecExpr::Let { name, value, body } => {
            let v = eval_body(value, bindings, bc, policy, new_tracer)?;
            let mut next = bindings.clone();
            next.insert(name.clone(), v);
            eval_body(body, &next, bc, policy, new_tracer)
        }
        SpecExpr::Not { expr } => match eval_body(expr, bindings, bc, policy, new_tracer)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => Err(format!("not on non-bool: {other:?}")),
        },
        SpecExpr::BinOp { op, lhs, rhs } => {
            // Short-circuit `and` / `or` so guard expressions like
            // `length(xs) == 0 or xs[0] > 0` don't evaluate the
            // second arm when the first already decides the result.
            // Matches the conventional boolean-operator semantics —
            // and the gate use case where the second arm may
            // legitimately error on the values the first arm
            // exists to filter out (#208 slice 2).
            if matches!(op, SpecOp::And | SpecOp::Or) {
                let a = eval_body(lhs, bindings, bc, policy, new_tracer)?;
                let av = match a {
                    Value::Bool(b) => b,
                    other => return Err(format!(
                        "{} on non-bool lhs: {other:?}", op.as_str())),
                };
                if matches!(op, SpecOp::And) && !av { return Ok(Value::Bool(false)); }
                if matches!(op, SpecOp::Or)  &&  av { return Ok(Value::Bool(true));  }
                let b = eval_body(rhs, bindings, bc, policy, new_tracer)?;
                return match b {
                    Value::Bool(bb) => Ok(Value::Bool(bb)),
                    other => Err(format!("{} on non-bool rhs: {other:?}", op.as_str())),
                };
            }
            let a = eval_body(lhs, bindings, bc, policy, new_tracer)?;
            let b = eval_body(rhs, bindings, bc, policy, new_tracer)?;
            apply_binop(*op, a, b)
        }
        SpecExpr::Call { func, args } => {
            let mut argv = Vec::new();
            for a in args { argv.push(eval_body(a, bindings, bc, policy, new_tracer)?); }
            // Spec-builtin list operations (#208). `length`, `head`,
            // and `tail` are intercepted before falling through to a
            // host VM call so specs can reason about list-shaped
            // bindings without the host program needing those names.
            // Identical name-shadowing behavior to lex's stdlib —
            // user code can still define a function `length` and
            // reference it from a spec, but a spec call to `length(xs)`
            // where `xs` is a `Value::List` resolves to the builtin.
            if let Some(v) = list_builtin(func, &argv) { return v; }
            let handler = DefaultHandler::new(policy.clone());
            let mut vm = Vm::with_handler(bc, Box::new(handler));
            if let Some(make_tracer) = new_tracer {
                vm.set_tracer(make_tracer());
            }
            vm.call(func, argv).map_err(|e| format!("call `{func}`: {e}"))
        }
        SpecExpr::Index { list, index } => {
            let xs = eval_body(list, bindings, bc, policy, new_tracer)?;
            let i = eval_body(index, bindings, bc, policy, new_tracer)?;
            list_index(xs, i)
        }
        SpecExpr::FieldAccess { value, field } => {
            // Drill into a record-typed binding (#208). Fails loudly
            // if the value isn't a record or the field is missing —
            // both indicate a spec/binding shape mismatch the agent
            // wants to know about, not silently default.
            let v = eval_body(value, bindings, bc, policy, new_tracer)?;
            match v {
                Value::Record(fields) => fields.get(field).cloned().ok_or_else(|| {
                    let known: Vec<&str> = fields.keys().map(String::as_str).collect();
                    format!("field `{field}` missing on record (have: {})", known.join(", "))
                }),
                other => Err(format!(
                    "field access `.{field}` on non-record: {}",
                    short_value(&other))),
            }
        }
    }
}

/// Spec-builtin list operations (#208). Returns `Some(result)` if
/// `func` names a builtin (`length`, `head`, `tail`) and the args
/// shape matches; returns `None` to indicate the call should fall
/// through to a host VM dispatch.
pub(crate) fn list_builtin(func: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match func {
        "length" => {
            if args.len() != 1 {
                return Some(Err(format!("length: expected 1 arg, got {}", args.len())));
            }
            match &args[0] {
                Value::List(xs) => Some(Ok(Value::Int(xs.len() as i64))),
                // Not a list — fall through to host dispatch in case
                // the user defined their own `length` function.
                _ => None,
            }
        }
        "head" => {
            if args.len() != 1 { return None; }
            match &args[0] {
                Value::List(xs) => Some(match xs.first() {
                    Some(v) => Ok(v.clone()),
                    None => Err("head: empty list".into()),
                }),
                _ => None,
            }
        }
        "tail" => {
            if args.len() != 1 { return None; }
            match &args[0] {
                Value::List(xs) => Some(match xs.split_first() {
                    Some((_, rest)) => Ok(Value::List(rest.to_vec())),
                    None => Err("tail: empty list".into()),
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn list_index(list: Value, index: Value) -> Result<Value, String> {
    let xs = match list {
        Value::List(xs) => xs,
        other => return Err(format!("index `[..]` on non-list: {}", short_value(&other))),
    };
    let i = match index {
        Value::Int(n) => n,
        other => return Err(format!("list index must be Int, got {}", short_value(&other))),
    };
    if i < 0 || (i as usize) >= xs.len() {
        return Err(format!(
            "list index {i} out of bounds (length {})", xs.len()));
    }
    Ok(xs[i as usize].clone())
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

    // ---- #199: optional tracer hook -----------------------------

    /// Minimal Tracer that records every enter_call name into a
    /// shared Vec. Avoids depending on lex-trace; soft-agent's
    /// real wiring uses `lex_trace::Recorder` + `Handle::clone`.
    struct CallRecorder {
        captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl lex_bytecode::vm::Tracer for CallRecorder {
        fn enter_call(&mut self, _node_id: &str, name: &str, _args: &[Value]) {
            self.captured.lock().unwrap().push(name.to_string());
        }
        fn enter_effect(&mut self, _: &str, _: &str, _: &str, _: &[Value]) {}
        fn exit_ok(&mut self, _: &Value) {}
        fn exit_err(&mut self, _: &str) {}
        fn exit_call_tail(&mut self) {}
        fn override_effect(&mut self, _: &str) -> Option<Value> { None }
    }

    #[test]
    fn traced_gate_captures_nested_call_events() {
        // Spec body calls `under_budget`, which itself calls
        // `projected_load` and `budget_total`. Without the tracer
        // hook, only the top-level Lex call appears in any
        // recorder; with it, the nested helpers do too.
        let host_src = r#"
            fn projected_load(active :: Int, delta :: Int) -> Int {
              active + delta
            }
            fn budget_total(budget :: Int, headroom :: Int) -> Int {
              budget + headroom
            }
            fn under_budget(active :: Int, delta :: Int, budget :: Int, headroom :: Int) -> Bool {
              projected_load(active, delta) <= budget_total(budget, headroom)
            }
        "#;
        let prog = parse_source(host_src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let bc = compile_program(&stages);

        let spec = parse_spec(r#"
            spec gated_budget {
              forall active :: Int, delta :: Int, budget :: Int, headroom :: Int :
                under_budget(active, delta, budget, headroom)
            }
        "#).unwrap();
        let bindings = b([
            ("active", Value::Int(40)),
            ("delta", Value::Int(15)),
            ("budget", Value::Int(60)),
            ("headroom", Value::Int(0)),
        ]);

        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let captured_for_factory = std::sync::Arc::clone(&captured);
        let v = evaluate_gate_compiled_traced(
            std::slice::from_ref(&spec),
            &bindings,
            &bc,
            move || Box::new(CallRecorder {
                captured: std::sync::Arc::clone(&captured_for_factory),
            }),
        );
        assert_eq!(v, GateVerdict::Allow);

        let calls = captured.lock().unwrap();
        // The Vm fires `enter_call` for sub-calls executed inside
        // the entry function's body, not for the host-driven
        // `Vm::call("under_budget", ...)` itself — that's the
        // host's contract. The point of the tracer hook is that
        // these *nested* helpers are visible at all; pre-#199
        // they were entirely opaque to the gate's recorder.
        for expected in ["projected_load", "budget_total"] {
            assert!(calls.iter().any(|c| c == expected),
                "expected `{expected}` in captured calls; got {:?}", *calls);
        }
    }

    #[test]
    fn untraced_gate_path_unchanged() {
        // The existing `evaluate_gate_compiled` API stays unaffected
        // by #199: same signature, same behavior. Pin this so future
        // refactors of the inner factory threading don't quietly
        // shift the public contract.
        let spec = parse_spec("spec ok { forall x :: Int : x + 1 > x }").unwrap();
        let v = evaluate_gate_compiled(
            std::slice::from_ref(&spec),
            &b([("x", Value::Int(5))]),
            &compile_program(&lex_ast::canonicalize_program(&parse_source("").unwrap())),
        );
        assert_eq!(v, GateVerdict::Allow);
    }

    // ---- #208: record-typed bindings + field access ------------------

    /// Build a `Value::Record` from `(field, value)` pairs.
    fn rec(fields: &[(&str, Value)]) -> Value {
        let mut m = indexmap::IndexMap::new();
        for (k, v) in fields {
            m.insert((*k).into(), v.clone());
        }
        Value::Record(m)
    }

    #[test]
    fn record_quantifier_type_parses() {
        // The header type uses the new record syntax. The body
        // doesn't have to use it — confirms the parser accepts the
        // `{ name :: Ty, ... }` shape independently of how the spec
        // body references the binding.
        let spec = parse_spec(r#"
            spec session_ok {
              forall s :: { used :: Int, ceiling :: Int } : true
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([
            ("s", rec(&[("used", Value::Int(0)), ("ceiling", Value::Int(100))])),
        ]), "");
        assert_eq!(v, GateVerdict::Allow);
    }

    #[test]
    fn field_access_drills_into_record_value() {
        // The headline #208 case: spec quantifies a record-shaped
        // binding *and* references its fields directly. Pre-#208
        // soft-agent had to flatten this via BindingsFn.
        let spec = parse_spec(r#"
            spec budget_ok {
              forall s :: { used :: Int, ceiling :: Int } :
                s.used <= s.ceiling
            }
        "#).unwrap();
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("s", rec(&[("used", Value::Int(40)), ("ceiling", Value::Int(100))])),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
        let deny = evaluate_gate(&[spec], &b([
            ("s", rec(&[("used", Value::Int(120)), ("ceiling", Value::Int(100))])),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn nested_record_field_access_works() {
        // `s.charge.power_drawn` — chained field access. Mirrors the
        // structured-state pattern that motivated the issue (see the
        // "active sessions, station.power_drawn ≤ station.budget"
        // example in #208's background).
        let spec = parse_spec(r#"
            spec station_ok {
              forall s :: { charge :: { power_drawn :: Int, budget :: Int } } :
                s.charge.power_drawn <= s.charge.budget
            }
        "#).unwrap();
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("s", rec(&[("charge", rec(&[
                ("power_drawn", Value::Int(50)),
                ("budget", Value::Int(80)),
            ]))])),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
    }

    #[test]
    fn missing_field_is_inconclusive_not_panic() {
        // Spec references `s.budget` but the binding has only `s.used`.
        // This is an agent/spec mismatch; surface as Inconclusive with a
        // diagnostic listing the available fields.
        let spec = parse_spec(r#"
            spec needs_budget {
              forall s :: { used :: Int, budget :: Int } : s.used <= s.budget
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([
            ("s", rec(&[("used", Value::Int(40))])),
        ]), "");
        match v {
            GateVerdict::Inconclusive { reason, .. } => {
                assert!(reason.contains("field `budget`"),
                    "reason should name the missing field; got: {reason}");
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    #[test]
    fn field_access_on_non_record_is_inconclusive() {
        // Catches the "spec author forgot the value was scalar" case.
        let spec = parse_spec(r#"
            spec wrong_shape {
              forall x :: Int : x.used > 0
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([("x", Value::Int(40))]), "");
        match v {
            GateVerdict::Inconclusive { reason, .. } => {
                assert!(reason.contains("non-record"),
                    "reason should call out non-record; got: {reason}");
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    // ---- #208 slice 2: list-typed bindings ---------------------------

    /// Build a `Value::List` from a slice of values.
    fn lst(items: &[Value]) -> Value {
        Value::List(items.to_vec())
    }

    #[test]
    fn list_quantifier_type_parses() {
        let spec = parse_spec(r#"
            spec ok {
              forall xs :: List[Int] : true
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([
            ("xs", lst(&[Value::Int(1), Value::Int(2)])),
        ]), "");
        assert_eq!(v, GateVerdict::Allow);
    }

    #[test]
    fn length_builtin_returns_list_length() {
        let spec = parse_spec(r#"
            spec at_least_one {
              forall xs :: List[Int] : length(xs) > 0
            }
        "#).unwrap();
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("xs", lst(&[Value::Int(7)])),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
        let deny = evaluate_gate(&[spec], &b([
            ("xs", lst(&[])),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn indexed_access_reads_list_element() {
        let spec = parse_spec(r#"
            spec head_positive {
              forall xs :: List[Int] : xs[0] > 0
            }
        "#).unwrap();
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("xs", lst(&[Value::Int(5), Value::Int(10)])),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
        let deny = evaluate_gate(&[spec], &b([
            ("xs", lst(&[Value::Int(0), Value::Int(10)])),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn head_and_tail_builtins_work() {
        // `head(xs) >= length(tail(xs))` — silly but exercises both
        // builtins together with a length() over the tail.
        let spec = parse_spec(r#"
            spec shape {
              forall xs :: List[Int] :
                length(xs) > 0 and head(xs) >= length(tail(xs))
            }
        "#).unwrap();
        // [3, 1, 2]: head=3, tail=[1,2] → length 2; 3 >= 2 ✓
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("xs", lst(&[Value::Int(3), Value::Int(1), Value::Int(2)])),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);
        // [1, 1, 2, 3]: head=1, tail=[1,2,3] → length 3; 1 >= 3 ✗
        let deny = evaluate_gate(&[spec], &b([
            ("xs", lst(&[Value::Int(1), Value::Int(1),
                         Value::Int(2), Value::Int(3)])),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn list_of_records_lets_specs_quantify_structured_collections() {
        // The pattern motivated by the issue's "for every charging
        // session in active_sessions, station.power_drawn ≤ station.budget"
        // example. The spec checks the *first* session's invariant —
        // a per-element forall is slice 3's territory; this slice
        // verifies the structural plumbing (List of Record + indexed
        // access + field access) composes.
        let spec = parse_spec(r#"
            spec first_session_within_budget {
              forall sessions :: List[{ power :: Int, budget :: Int }] :
                length(sessions) == 0 or sessions[0].power <= sessions[0].budget
            }
        "#).unwrap();
        let mut session = indexmap::IndexMap::new();
        session.insert("power".into(), Value::Int(50));
        session.insert("budget".into(), Value::Int(80));
        let allow = evaluate_gate(std::slice::from_ref(&spec), &b([
            ("sessions", Value::List(vec![Value::Record(session.clone())])),
        ]), "");
        assert_eq!(allow, GateVerdict::Allow);

        let mut over = indexmap::IndexMap::new();
        over.insert("power".into(), Value::Int(120));
        over.insert("budget".into(), Value::Int(80));
        let deny = evaluate_gate(&[spec], &b([
            ("sessions", Value::List(vec![Value::Record(over)])),
        ]), "");
        assert!(matches!(deny, GateVerdict::Deny { .. }));
    }

    #[test]
    fn empty_list_passes_when_predicate_is_vacuous() {
        // Verifies the `length(xs) == 0 or ...` short-circuit pattern
        // used to make per-list predicates well-defined on empties.
        let spec = parse_spec(r#"
            spec ok_or_empty {
              forall xs :: List[Int] : length(xs) == 0 or xs[0] > 0
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([("xs", lst(&[]))]), "");
        assert_eq!(v, GateVerdict::Allow);
    }

    #[test]
    fn out_of_bounds_index_is_inconclusive() {
        let spec = parse_spec(r#"
            spec needs_two {
              forall xs :: List[Int] : xs[1] > 0
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([
            ("xs", lst(&[Value::Int(5)])),  // length 1; xs[1] OOB
        ]), "");
        match v {
            GateVerdict::Inconclusive { reason, .. } => {
                assert!(reason.contains("out of bounds"),
                    "expected OOB diagnostic; got: {reason}");
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }

    #[test]
    fn head_of_empty_list_is_inconclusive() {
        let spec = parse_spec(r#"
            spec head_pos {
              forall xs :: List[Int] : head(xs) > 0
            }
        "#).unwrap();
        let v = evaluate_gate(&[spec], &b([("xs", lst(&[]))]), "");
        match v {
            GateVerdict::Inconclusive { reason, .. } => {
                assert!(reason.contains("empty list"),
                    "expected empty-list diagnostic; got: {reason}");
            }
            other => panic!("expected Inconclusive, got {other:?}"),
        }
    }
}
