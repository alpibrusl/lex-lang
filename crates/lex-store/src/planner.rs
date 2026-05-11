//! `lex plan` — cost-aware path planner over the call graph (#307).
//!
//! Given a `goal` function and either an explicit `max_cost` or a
//! session id whose remaining budget caps the spend, enumerate every
//! linear call chain from `goal` to a leaf (a function that calls no
//! other user-defined function on the branch head). Each chain is
//! scored by the sum of `[budget(N)]` declarations along it, and the
//! result is sorted cheapest-first.
//!
//! The planner is **advisory** — it doesn't execute anything. Agents
//! consult it to pick the cheapest reachable path that fits in their
//! remaining budget; downstream policy (`#292`'s gate) is what
//! ultimately admits or refuses an op.

use crate::{Store, StoreError};
use lex_ast::{CExpr, Stage};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// One linear chain from `goal` to a leaf, with its total cost and
/// the union of effects along it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanPath {
    /// Functions in call order — `chain[0]` is always `goal`.
    pub chain: Vec<String>,
    /// Sum of declared `[budget(N)]` for every fn in `chain`.
    /// Recursive self-calls are counted **once** (the cycle is broken
    /// at the second visit) — see `expand_paths` for the visited-set.
    pub total_cost: u64,
    /// `true` iff `total_cost <= effective_cap` (whichever of
    /// `max_cost` and the session-remaining is smaller). Always
    /// `true` when no cap applies.
    pub fits: bool,
    /// Union of effect names declared on every fn in the chain
    /// (excluding the `budget` pseudo-effect, which is the cost
    /// dimension itself).
    pub effects: BTreeSet<String>,
}

/// Result envelope returned by [`Store::plan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    /// `Some(id)` when the planner was called with `--intent`/
    /// `session_id`; `None` for the bare `--max-cost` flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Budget remaining for `session_id`, when one was supplied
    /// and a cap is configured. `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_budget: Option<i64>,
    /// Whichever of `max_cost` and `remaining_budget` is smaller.
    /// `None` when neither cap applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_cap: Option<u64>,
    /// Paths sorted cheapest-first.
    pub paths: Vec<PlanPath>,
}

/// Per-function summary extracted once per branch head.
struct FnInfo {
    budget_cost: u64,
    effects: BTreeSet<String>,
    calls: Vec<String>,
}

impl Store {
    /// `lex plan` (#307). See module docs.
    pub fn plan(
        &self,
        branch: &str,
        goal: &str,
        max_cost: Option<u64>,
        session_id: Option<&str>,
    ) -> Result<Plan, StoreError> {
        // Build per-fn summaries from the branch head's active set.
        let head = self
            .branch_head(branch)
            .map_err(|e| StoreError::Io(std::io::Error::other(format!("branch_head: {e}"))))?;
        let mut fns: BTreeMap<String, FnInfo> = BTreeMap::new();
        for stage_id in head.values() {
            let Ok(Stage::FnDecl(fd)) = self.get_ast(stage_id) else { continue };
            let mut effects = BTreeSet::new();
            let mut budget_cost: u64 = 0;
            for e in &fd.effects {
                if e.name == "budget" {
                    if let Some(lex_ast::EffectArg::Int { value }) = &e.arg {
                        budget_cost = budget_cost.saturating_add(*value as u64);
                    }
                } else {
                    effects.insert(e.name.clone());
                }
            }
            let mut calls = Vec::new();
            collect_call_targets(&fd.body, &mut calls);
            fns.entry(fd.name.clone()).or_insert(FnInfo {
                budget_cost,
                effects,
                calls,
            });
        }

        // Resolve the session's remaining budget (if any).
        let (remaining_budget, session_id_out) = if let Some(sid) = session_id {
            let sb = self.session_budget(sid)?;
            (sb.remaining, Some(sid.to_string()))
        } else {
            (None, None)
        };

        // Effective cap = min(max_cost, max(remaining, 0))
        let effective_cap: Option<u64> = match (max_cost, remaining_budget) {
            (Some(m), Some(r)) => Some(m.min(r.max(0) as u64)),
            (Some(m), None) => Some(m),
            (None, Some(r)) => Some(r.max(0) as u64),
            (None, None) => None,
        };

        let mut paths: Vec<PlanPath> = Vec::new();
        if fns.contains_key(goal) {
            expand_paths(goal, &fns, &mut Vec::new(), &mut HashSet::new(), &mut paths);
        }
        // Cheapest-first, tie-break by chain length then alphabetical.
        paths.sort_by(|a, b| {
            a.total_cost
                .cmp(&b.total_cost)
                .then_with(|| a.chain.len().cmp(&b.chain.len()))
                .then_with(|| a.chain.cmp(&b.chain))
        });
        // Stamp `fits` against the resolved cap.
        for p in &mut paths {
            p.fits = effective_cap.is_none_or(|cap| p.total_cost <= cap);
        }

        Ok(Plan {
            goal: goal.to_string(),
            session_id: session_id_out,
            remaining_budget,
            effective_cap,
            paths,
        })
    }
}

/// Walk `expr` and append every direct call to a top-level fn (a
/// `Call { callee = Var { name } }` shape) into `out`. Module-method
/// calls (`io.print`) and closure calls are intentionally skipped —
/// they don't reference user-defined fns by name in the call graph.
fn collect_call_targets(expr: &CExpr, out: &mut Vec<String>) {
    match expr {
        CExpr::Call { callee, args } => {
            if let CExpr::Var { name } = callee.as_ref() {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
            collect_call_targets(callee, out);
            for a in args {
                collect_call_targets(a, out);
            }
        }
        CExpr::Let { value, body, .. } => {
            collect_call_targets(value, out);
            collect_call_targets(body, out);
        }
        CExpr::Match { scrutinee, arms } => {
            collect_call_targets(scrutinee, out);
            for arm in arms {
                collect_call_targets(&arm.body, out);
            }
        }
        CExpr::Block { statements, result } => {
            for s in statements {
                collect_call_targets(s, out);
            }
            collect_call_targets(result, out);
        }
        CExpr::Constructor { args, .. } => {
            for a in args {
                collect_call_targets(a, out);
            }
        }
        CExpr::RecordLit { fields } => {
            for f in fields {
                collect_call_targets(&f.value, out);
            }
        }
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for i in items {
                collect_call_targets(i, out);
            }
        }
        CExpr::FieldAccess { value, .. } => collect_call_targets(value, out),
        CExpr::Lambda { body, .. } => collect_call_targets(body, out),
        CExpr::BinOp { lhs, rhs, .. } => {
            collect_call_targets(lhs, out);
            collect_call_targets(rhs, out);
        }
        CExpr::UnaryOp { expr, .. } => collect_call_targets(expr, out),
        CExpr::Return { value } => collect_call_targets(value, out),
        CExpr::Literal { .. } | CExpr::Var { .. } => {}
    }
}

/// DFS enumeration of paths from `current` to every reachable leaf,
/// breaking recursion at the second visit so a `recur` function's
/// cost is counted once.
fn expand_paths(
    current: &str,
    fns: &BTreeMap<String, FnInfo>,
    chain: &mut Vec<String>,
    visited: &mut HashSet<String>,
    out: &mut Vec<PlanPath>,
) {
    chain.push(current.to_string());
    let newly_inserted = visited.insert(current.to_string());

    let Some(info) = fns.get(current) else {
        // Unknown callee (stdlib or external) — terminate the chain
        // here. The leaf itself contributes nothing to cost/effects.
        emit_path(chain, fns, out);
        chain.pop();
        if newly_inserted {
            visited.remove(current);
        }
        return;
    };

    // Pick the in-scope, not-yet-visited callees so we don't expand
    // recursive cycles.
    let next: Vec<&String> = info
        .calls
        .iter()
        .filter(|c| !visited.contains(*c))
        .collect();
    if next.is_empty() {
        emit_path(chain, fns, out);
    } else {
        for callee in next {
            expand_paths(callee, fns, chain, visited, out);
        }
    }

    chain.pop();
    if newly_inserted {
        visited.remove(current);
    }
}

fn emit_path(chain: &[String], fns: &BTreeMap<String, FnInfo>, out: &mut Vec<PlanPath>) {
    let mut total_cost: u64 = 0;
    let mut effects: BTreeSet<String> = BTreeSet::new();
    for name in chain {
        if let Some(info) = fns.get(name) {
            total_cost = total_cost.saturating_add(info.budget_cost);
            for e in &info.effects {
                effects.insert(e.clone());
            }
        }
    }
    out.push(PlanPath {
        chain: chain.to_vec(),
        total_cost,
        fits: true, // patched by caller against effective_cap
        effects,
    });
}
