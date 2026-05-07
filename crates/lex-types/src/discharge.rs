//! Static discharge of refinement-type predicates (#209 slice 2).
//!
//! Given a refined parameter type `Int{x | x > 0}` and a call-site
//! argument `5`, this module evaluates the predicate with the binding
//! bound to the argument's value and returns whether the call site
//! satisfies, violates, or can't statically decide the constraint.
//!
//! Scope (deliberately small for v1):
//!
//! - **Argument shape**: only `CLit` literals (`Int`, `Float`, `Bool`,
//!   `Str`). Anything else — variables, calls, arithmetic — defers
//!   to slice 3's runtime residual check.
//! - **Predicate shape**: literals, the bound variable, binary
//!   arithmetic / comparison, boolean `and` / `or` with short-circuit,
//!   `not`. Anything else defers.
//! - **Free variables**: the predicate must reference *only* its
//!   binding. Other identifiers (`balance`, `ceiling`, etc.) defer
//!   — slice 2 doesn't try to evaluate them. Slice 3 will plumb
//!   call-site context bindings.
//!
//! The deliberate fall-through to "deferred" is what makes this slice
//! ship-able: anything we can't reason about cleanly stays a
//! type-check pass and waits for slice 3's runtime check, rather
//! than blocking type-check on cases we can't yet handle.

use lex_ast::{CExpr, CLit};

#[derive(Debug, Clone, PartialEq)]
pub enum DischargeOutcome {
    /// Predicate evaluated to `true` with the binding bound to the
    /// argument value. Static check succeeds; no runtime work.
    Proved,
    /// Predicate evaluated to `false`. The call-site argument
    /// definitely violates the refinement — surface as a type error.
    Refuted { reason: String },
    /// Couldn't decide statically. Slice 3 will emit a residual
    /// runtime check at the call boundary; this slice just lets
    /// type-check pass.
    Deferred { reason: String },
}

/// Try to discharge `predicate` with `binding_name` bound to `arg`.
/// `arg` is the CallExpr's argument expression — only literal forms
/// (`CLit`) participate in static discharge; anything else defers.
pub fn try_discharge(
    predicate: &CExpr,
    binding_name: &str,
    arg: &CExpr,
) -> DischargeOutcome {
    let v = match arg_to_concrete(arg) {
        Some(v) => v,
        None => return DischargeOutcome::Deferred {
            reason: "argument is not a literal; static discharge can't \
                     evaluate it (slice 3 will add a runtime check)".into(),
        },
    };
    match eval(predicate, binding_name, &v) {
        Ok(Concrete::Bool(true)) => DischargeOutcome::Proved,
        Ok(Concrete::Bool(false)) => DischargeOutcome::Refuted {
            reason: format!(
                "predicate failed for {} = {}",
                binding_name, v.show()),
        },
        Ok(other) => DischargeOutcome::Deferred {
            reason: format!("predicate didn't reduce to a Bool (got {})",
                other.show()),
        },
        Err(e) => DischargeOutcome::Deferred { reason: e },
    }
}

/// Reduce a call-site argument expression to a `Concrete` value if
/// it's a literal or a sign-flipped literal. Lex parses `-5` as
/// `UnaryOp { op: "-", expr: Literal { Int(5) } }` rather than a
/// negative literal, so we fold that one shape inline. Anything
/// more involved (arithmetic, var refs) defers — slice 3 territory.
fn arg_to_concrete(e: &CExpr) -> Option<Concrete> {
    match e {
        CExpr::Literal { value } => Some(Concrete::from_lit(value)),
        CExpr::UnaryOp { op, expr } if op == "-" => match expr.as_ref() {
            CExpr::Literal { value: CLit::Int { value } } =>
                Some(Concrete::Int(-value)),
            CExpr::Literal { value: CLit::Float { value } } => {
                value.parse::<f64>().ok().map(|f| Concrete::Float(-f))
            }
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Concrete {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

impl Concrete {
    fn from_lit(l: &CLit) -> Self {
        match l {
            CLit::Int { value } => Concrete::Int(*value),
            CLit::Float { value } => Concrete::Float(value.parse().unwrap_or(0.0)),
            CLit::Bool { value } => Concrete::Bool(*value),
            CLit::Str { value } => Concrete::Str(value.clone()),
            // Bytes / Unit are unusual in refinement contexts; surface
            // as Unit so the predicate evaluator defers cleanly.
            _ => Concrete::Bool(false),
        }
    }
    fn show(&self) -> String {
        match self {
            Concrete::Int(n) => format!("{n}"),
            Concrete::Float(f) => format!("{f}"),
            Concrete::Bool(b) => format!("{b}"),
            Concrete::Str(s) => format!("{s:?}"),
        }
    }
}

/// Tiny tree-walk evaluator for a predicate `CExpr` with a single
/// in-scope binding. Returns `Err` for unsupported forms (caller
/// folds these into `Deferred`).
fn eval(e: &CExpr, name: &str, value: &Concrete) -> Result<Concrete, String> {
    match e {
        CExpr::Literal { value: lit } => Ok(Concrete::from_lit(lit)),
        CExpr::Var { name: n } => {
            if n == name { Ok(value.clone()) }
            else { Err(format!("predicate references free var `{n}`; \
                                slice 2 only supports the binding itself")) }
        }
        CExpr::UnaryOp { op, expr } => {
            let v = eval(expr, name, value)?;
            match (op.as_str(), v) {
                ("not", Concrete::Bool(b)) => Ok(Concrete::Bool(!b)),
                ("-",   Concrete::Int(n)) => Ok(Concrete::Int(-n)),
                ("-",   Concrete::Float(n)) => Ok(Concrete::Float(-n)),
                (o, v) => Err(format!("unsupported unary `{o}` on {}", v.show())),
            }
        }
        CExpr::BinOp { op, lhs, rhs } => {
            // Short-circuit `and` / `or` so the right side never
            // gets evaluated when the left already decides.
            if op == "and" || op == "or" {
                let l = eval(lhs, name, value)?;
                let lb = match l {
                    Concrete::Bool(b) => b,
                    other => return Err(format!("`{op}` on non-bool: {}", other.show())),
                };
                if op == "and" && !lb { return Ok(Concrete::Bool(false)); }
                if op == "or"  &&  lb { return Ok(Concrete::Bool(true));  }
                let r = eval(rhs, name, value)?;
                return match r {
                    Concrete::Bool(b) => Ok(Concrete::Bool(b)),
                    other => Err(format!("`{op}` on non-bool: {}", other.show())),
                };
            }
            let l = eval(lhs, name, value)?;
            let r = eval(rhs, name, value)?;
            apply_binop(op, &l, &r)
        }
        // Anything else (Call, Let, Match, FieldAccess, Lambda, Block,
        // Constructors, Records, Tuples, Lists, Return) is out of
        // slice-2 scope. Slice 3's runtime check handles these by
        // falling back to actual evaluation under the host VM.
        _ => Err(format!("unsupported predicate node: {e:?}")),
    }
}

fn apply_binop(op: &str, l: &Concrete, r: &Concrete) -> Result<Concrete, String> {
    use Concrete::*;
    match (op, l, r) {
        ("+", Int(a), Int(b)) => Ok(Int(a + b)),
        ("-", Int(a), Int(b)) => Ok(Int(a - b)),
        ("*", Int(a), Int(b)) => Ok(Int(a * b)),
        ("/", Int(a), Int(b)) if *b != 0 => Ok(Int(a / b)),
        ("%", Int(a), Int(b)) if *b != 0 => Ok(Int(a % b)),
        ("+", Float(a), Float(b)) => Ok(Float(a + b)),
        ("-", Float(a), Float(b)) => Ok(Float(a - b)),
        ("*", Float(a), Float(b)) => Ok(Float(a * b)),
        ("/", Float(a), Float(b)) => Ok(Float(a / b)),

        ("==", a, b) => Ok(Bool(a == b)),
        ("!=", a, b) => Ok(Bool(a != b)),

        ("<",  Int(a), Int(b)) => Ok(Bool(a < b)),
        ("<=", Int(a), Int(b)) => Ok(Bool(a <= b)),
        (">",  Int(a), Int(b)) => Ok(Bool(a > b)),
        (">=", Int(a), Int(b)) => Ok(Bool(a >= b)),

        ("<",  Float(a), Float(b)) => Ok(Bool(a < b)),
        ("<=", Float(a), Float(b)) => Ok(Bool(a <= b)),
        (">",  Float(a), Float(b)) => Ok(Bool(a > b)),
        (">=", Float(a), Float(b)) => Ok(Bool(a >= b)),

        (op, a, b) => Err(format!(
            "unsupported binop `{op}` on {} and {}", a.show(), b.show())),
    }
}
