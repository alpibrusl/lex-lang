//! Mutation analysis (§13.3 acceptance #4).
//!
//! Core's small IR for mutable code: `let mut`, `:=` reassignment, and
//! `for` loops. The analyzer tracks which bindings originate from a
//! `mut` (or are assigned from a mut-tainted value) and rejects any
//! `Return` whose value is mut-tainted.
//!
//! This honors §13.3 rule 1: "`mut` bindings cannot escape a stage.
//! Returning a value computed via mutation copies it to a new immutable
//! value at the stage boundary." For Phase 2 we surface the violation
//! at compile time; the future codegen pass will emit the copy when
//! the rule allows it (e.g. returning a fresh `mut` accumulator).

use crate::error::CoreError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Tiny Core IR for mutation flow. We only care about the structure
/// that affects taint propagation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "node")]
pub enum CoreExpr {
    /// Reference an existing binding.
    Var { name: String },
    /// Numeric / boolean literal — not relevant to taint.
    Lit,
    /// Pure binding.
    Let { name: String, value: Box<CoreExpr>, body: Box<CoreExpr> },
    /// Mutable binding. Subsequent `Assign` may rebind it.
    LetMut { name: String, value: Box<CoreExpr>, body: Box<CoreExpr> },
    /// Reassign a (mutable) binding, then continue with `body`.
    Assign { name: String, value: Box<CoreExpr>, body: Box<CoreExpr> },
    /// `for i in lo..hi { body }; result` — `body` runs `hi-lo` times,
    /// `result` is the final expression. `body` typically `Assign`s
    /// the accumulator.
    For {
        var: String,
        lo: Box<CoreExpr>,
        hi: Box<CoreExpr>,
        body: Box<CoreExpr>,
        result: Box<CoreExpr>,
    },
    /// Tail position. The escape analysis flags mut-tainted values here.
    Return { value: Box<CoreExpr> },
    /// Sequence of two expressions; the first is evaluated for effect.
    Seq { first: Box<CoreExpr>, then: Box<CoreExpr> },
}

/// Run the escape analysis. Returns the offending `(stage, name)` if a
/// mut binding flows into a `Return`.
pub fn check_no_mut_return(stage_name: &str, body: &CoreExpr) -> Result<(), CoreError> {
    let mut env: IndexMap<String, Taint> = IndexMap::new();
    walk(stage_name, body, &mut env)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Taint { Pure, Mut }

fn walk(stage: &str, e: &CoreExpr, env: &mut IndexMap<String, Taint>) -> Result<(), CoreError> {
    match e {
        CoreExpr::Var { .. } | CoreExpr::Lit => Ok(()),
        CoreExpr::Let { name, value, body } => {
            walk(stage, value, env)?;
            let t = if expr_taint(value, env) == Taint::Mut { Taint::Mut } else { Taint::Pure };
            with_binding(env, name, t, |env| walk(stage, body, env))
        }
        CoreExpr::LetMut { name, value, body } => {
            walk(stage, value, env)?;
            with_binding(env, name, Taint::Mut, |env| walk(stage, body, env))
        }
        CoreExpr::Assign { name, value, body } => {
            walk(stage, value, env)?;
            // The assigned binding is now (still) mut-tainted.
            env.insert(name.clone(), Taint::Mut);
            walk(stage, body, env)
        }
        CoreExpr::For { var, lo, hi, body, result } => {
            walk(stage, lo, env)?;
            walk(stage, hi, env)?;
            with_binding(env, var, Taint::Pure, |env| walk(stage, body, env))?;
            walk(stage, result, env)
        }
        CoreExpr::Seq { first, then } => {
            walk(stage, first, env)?;
            walk(stage, then, env)
        }
        CoreExpr::Return { value } => {
            walk(stage, value, env)?;
            // §13.3 rule 1: mut may not escape the stage.
            if let Some(name) = mut_origin(value, env) {
                return Err(CoreError::MutEscape {
                    at: "return".into(),
                    stage: stage.to_string(),
                    name,
                });
            }
            Ok(())
        }
    }
}

fn with_binding<F, R>(env: &mut IndexMap<String, Taint>, name: &str, t: Taint, f: F) -> R
where F: FnOnce(&mut IndexMap<String, Taint>) -> R
{
    let prev = env.insert(name.to_string(), t);
    let r = f(env);
    match prev {
        Some(p) => { env.insert(name.to_string(), p); }
        None => { env.shift_remove(name); }
    }
    r
}

/// What's the worst-case taint of evaluating `e` under `env`?
fn expr_taint(e: &CoreExpr, env: &IndexMap<String, Taint>) -> Taint {
    match e {
        CoreExpr::Var { name } => env.get(name).copied().unwrap_or(Taint::Pure),
        CoreExpr::Lit => Taint::Pure,
        CoreExpr::Let { body, .. } | CoreExpr::LetMut { body, .. } | CoreExpr::Assign { body, .. } => {
            expr_taint(body, env)
        }
        CoreExpr::For { result, .. } => expr_taint(result, env),
        CoreExpr::Seq { then, .. } => expr_taint(then, env),
        CoreExpr::Return { value } => expr_taint(value, env),
    }
}

/// If `e` reduces to a mut-tainted Var, return its name (for the error message).
fn mut_origin(e: &CoreExpr, env: &IndexMap<String, Taint>) -> Option<String> {
    match e {
        CoreExpr::Var { name } => match env.get(name) {
            Some(Taint::Mut) => Some(name.clone()),
            _ => None,
        },
        CoreExpr::Let { body, .. } | CoreExpr::LetMut { body, .. } | CoreExpr::Assign { body, .. } => {
            mut_origin(body, env)
        }
        CoreExpr::For { result, .. } => mut_origin(result, env),
        CoreExpr::Seq { then, .. } => mut_origin(then, env),
        _ => None,
    }
}
