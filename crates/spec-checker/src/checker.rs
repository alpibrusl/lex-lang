//! Randomized checker that drives the Lex VM with generated inputs.

use crate::ast::*;
use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProofStatus { Proved, Counterexample, Inconclusive }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub method: String,
    pub trials: u32,
    /// On counterexample, the failing input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterexample: Option<IndexMap<String, serde_json::Value>>,
    /// On inconclusive, why we couldn't decide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub spec_id: String,
    pub status: ProofStatus,
    pub evidence: Evidence,
}

/// Check a spec against a Lex program. The program must define the
/// function the spec refers to (by name).
///
/// `trials`: number of random samples for the randomized strategy.
/// Recommended ≥ 1000 for honest "proved" claims; small numbers (e.g. 10)
/// are useful for fast smoke tests.
pub fn check_spec(spec: &Spec, lex_source: &str, trials: u32) -> CheckResult {
    let spec_id = spec_id(spec);
    let prog = match parse_source(lex_source) {
        Ok(p) => p,
        Err(e) => return inconclusive(spec_id, format!("parse: {e}")),
    };
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return inconclusive(spec_id, format!("typecheck: {errs:?}"));
    }
    let bc = compile_program(&stages);

    // Skip Float-only specs in the randomized strategy: float spaces are
    // huge and counterexamples don't generalize. Report inconclusive
    // honestly per spec §14.5.
    if spec.quantifiers.iter().any(|q| q.ty == SpecType::Float) {
        return CheckResult {
            spec_id,
            status: ProofStatus::Inconclusive,
            evidence: Evidence {
                method: "randomized".into(),
                trials: 0,
                counterexample: None,
                note: Some("randomized search inconclusive on Float quantifiers; use SMT (see to_smtlib)".into()),
            },
        };
    }

    let mut rng = DetRng::new(seed_from_spec(spec));
    let policy = Policy::permissive();

    for trial in 0..trials {
        let mut bindings = IndexMap::new();
        let mut skip = false;
        for q in &spec.quantifiers {
            let v = sample(&q.ty, &mut rng);
            bindings.insert(q.name.clone(), v);
            // Apply per-quantifier constraint.
            if let Some(c) = &q.constraint {
                match eval(c, &bindings, &bc, &policy) {
                    Ok(Value::Bool(true)) => {}
                    Ok(Value::Bool(false)) => { skip = true; break; }
                    Ok(_) => return inconclusive(spec_id, format!("constraint did not return Bool (trial {trial})")),
                    Err(e) => return inconclusive(spec_id, format!("constraint eval failed: {e}")),
                }
            }
        }
        if skip { continue; }

        match eval(&spec.body, &bindings, &bc, &policy) {
            Ok(Value::Bool(true)) => continue,
            Ok(Value::Bool(false)) => {
                return CheckResult {
                    spec_id,
                    status: ProofStatus::Counterexample,
                    evidence: Evidence {
                        method: "randomized".into(),
                        trials: trial + 1,
                        counterexample: Some(bindings_to_json(&bindings)),
                        note: None,
                    },
                };
            }
            Ok(other) => return inconclusive(spec_id, format!("body returned non-bool: {other:?}")),
            Err(e) => return inconclusive(spec_id, format!("body eval failed: {e}")),
        }
    }

    CheckResult {
        spec_id,
        status: ProofStatus::Proved,
        evidence: Evidence {
            method: "randomized".into(),
            trials,
            counterexample: None,
            note: Some(format!("survived {trials} random trials; not a deductive proof")),
        },
    }
}

fn inconclusive(spec_id: String, note: impl Into<String>) -> CheckResult {
    CheckResult {
        spec_id,
        status: ProofStatus::Inconclusive,
        evidence: Evidence {
            method: "randomized".into(),
            trials: 0,
            counterexample: None,
            note: Some(note.into()),
        },
    }
}

fn spec_id(spec: &Spec) -> String {
    use sha2::{Digest, Sha256};
    let v = serde_json::to_value(spec).unwrap();
    let s = lex_ast::canon_json::to_canonical_string(&v);
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let r = h.finalize();
    let mut hex = String::with_capacity(64);
    for b in r { hex.push_str(&format!("{:02x}", b)); }
    hex
}

fn seed_from_spec(spec: &Spec) -> u64 {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(spec.name.as_bytes());
    let r = h.finalize();
    u64::from_le_bytes(r[..8].try_into().unwrap())
}

/// Tiny deterministic xorshift RNG, no external dep.
struct DetRng { state: u64 }
impl DetRng {
    fn new(seed: u64) -> Self { Self { state: seed.max(1) } }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
    fn next_i64_in(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo { return lo; }
        let span = (hi - lo) as u64;
        lo + (self.next_u64() % span) as i64
    }
}

fn sample(ty: &SpecType, rng: &mut DetRng) -> Value {
    match ty {
        SpecType::Int => Value::Int(rng.next_i64_in(-1000, 1001)),
        SpecType::Float => {
            let n = rng.next_u64() as f64;
            Value::Float((n / u64::MAX as f64) * 2000.0 - 1000.0)
        }
        SpecType::Bool => Value::Bool(rng.next_u64() & 1 == 0),
        SpecType::Str => {
            let n = rng.next_u64() % 6;
            Value::Str(("ab".repeat(n as usize)).chars().take(n as usize).collect())
        }
        // #208: random-input sampling on records recurses field by field.
        // The gate-evaluation path (which is what soft-agent uses) bypasses
        // this — agents pass concrete record values via bindings — but the
        // offline `check_spec` random-input prover needs a sampler too.
        SpecType::Record { fields } => {
            let mut out = indexmap::IndexMap::new();
            for (name, fty) in fields {
                out.insert(name.clone(), sample(fty, rng));
            }
            Value::Record(out)
        }
    }
}

fn eval(
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
            .ok_or_else(|| format!("unbound spec var `{name}`")),
        SpecExpr::Let { name, value, body } => {
            let v = eval(value, bindings, bc, policy)?;
            let mut next = bindings.clone();
            next.insert(name.clone(), v);
            eval(body, &next, bc, policy)
        }
        SpecExpr::Not { expr } => match eval(expr, bindings, bc, policy)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => Err(format!("not on non-bool: {other:?}")),
        },
        SpecExpr::BinOp { op, lhs, rhs } => {
            let a = eval(lhs, bindings, bc, policy)?;
            let b = eval(rhs, bindings, bc, policy)?;
            apply_binop(*op, a, b)
        }
        SpecExpr::Call { func, args } => {
            // Materialize args.
            let mut argv = Vec::new();
            for a in args { argv.push(eval(a, bindings, bc, policy)?); }
            // Run the Lex function with a fresh VM (cheap: program is shared).
            let handler = DefaultHandler::new(policy.clone());
            let mut vm = Vm::with_handler(bc, Box::new(handler));
            vm.call(func, argv).map_err(|e| format!("call `{func}`: {e}"))
        }
        SpecExpr::FieldAccess { value, field } => {
            // #208: random-input prover supports field access on records,
            // mirroring the gate path. The two evaluators stay in sync —
            // a spec that holds for a concrete record at runtime should
            // also hold for randomly-sampled records of the same shape.
            let v = eval(value, bindings, bc, policy)?;
            match v {
                Value::Record(fields) => fields.get(field).cloned().ok_or_else(|| {
                    let known: Vec<&str> = fields.keys().map(String::as_str).collect();
                    format!("field `{field}` missing on record (have: {})", known.join(", "))
                }),
                other => Err(format!("field access `.{field}` on non-record: {other:?}")),
            }
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

fn bindings_to_json(b: &IndexMap<String, Value>) -> IndexMap<String, serde_json::Value> {
    let mut out = IndexMap::new();
    for (k, v) in b { out.insert(k.clone(), value_to_json(v)); }
    out
}

fn value_to_json(v: &Value) -> serde_json::Value { v.to_json() }
