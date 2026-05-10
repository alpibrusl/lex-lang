//! Per-session budget ledger (#292 slice 1).
//!
//! Today `OperationKind::budget_delta()` records `(from, to)`
//! budget pairs on every op that touches a `[budget(N)]`-bearing
//! function. `lex audit --budget` already rolls those up per
//! signature. What's missing is the per-session aggregate: "how
//! much budget did session X cause to be spent across all the ops
//! it authored?"
//!
//! This module is the read-only ledger. Slice 2 layers a
//! `policy.json` cap on top; slice 3 wires the apply-path gate.
//!
//! # Spend model
//!
//! For each op tagged with an `intent_id` resolving to a session:
//!
//! - `AddFunction` with `budget_cost = Some(n)` contributes `n`.
//! - `ModifyBody` / `ChangeEffectSig` / `ReplaceMatchArm` /
//!   `RenameLocal` / `InlineLet` with `(from_budget, to_budget)`
//!   contribute `max(0, to - from)` (only budget *increases*
//!   count toward spend; refactor-to-cheaper doesn't refund).
//! - Ops without `intent_id` are excluded — there's no session to
//!   attribute them to.
//!
//! # Cost
//!
//! Single walk of the op log + one `IntentLog::get` per distinct
//! intent. For studies past ~100k ops, slice 2 will add an on-disk
//! cache keyed by `(session_id, head_op)` so re-reads are O(1).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::store::{Store, StoreError};

/// Rollup of a single session's budget spend.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionBudget {
    pub session_id: String,
    /// Sum of monotonic budget cost over all ops in this session.
    pub spent: u64,
    /// How many ops were attributed to this session (only those
    /// that contributed a non-zero increment count).
    pub op_count: usize,
}

impl Store {
    /// Compute the budget spent by the given `session_id` across
    /// every op currently reachable from any branch head. Returns
    /// `(spent: 0, op_count: 0)` for unknown sessions.
    pub fn session_budget(&self, session_id: &str) -> Result<SessionBudget, StoreError> {
        let all = self.all_session_budgets()?;
        Ok(all.into_iter()
            .find(|b| b.session_id == session_id)
            .unwrap_or(SessionBudget {
                session_id: session_id.into(),
                spent: 0,
                op_count: 0,
            }))
    }

    /// Compute per-session budget rollups across every branch.
    /// Returns one entry per distinct session that contributed at
    /// least one budget-bearing op. Sorted by `session_id` so the
    /// output is deterministic.
    pub fn all_session_budgets(&self) -> Result<Vec<SessionBudget>, StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let intent_log = lex_vcs::IntentLog::open(self.root())?;

        // Collect the union of ops reachable from every branch
        // head. Walking each branch separately and unioning by
        // op_id avoids double-counting on diamond histories.
        let mut visited: std::collections::BTreeSet<lex_vcs::OpId> = Default::default();
        let mut records: Vec<lex_vcs::OperationRecord> = Vec::new();
        for branch_name in self.list_branches()? {
            let Some(branch) = self.get_branch(&branch_name)? else { continue };
            let Some(head) = branch.head_op else { continue };
            for rec in log.walk_back(&head, None)? {
                if visited.insert(rec.op_id.clone()) {
                    records.push(rec);
                }
            }
        }

        // Walk records → resolve intent → resolve session → tally.
        // Intent lookups are cached so we don't hit the IntentLog
        // once per op when many ops share an intent.
        let mut intent_to_session: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut buckets: BTreeMap<String, (u64, usize)> = BTreeMap::new();
        for rec in &records {
            let Some(intent_id) = rec.op.intent_id.as_deref() else { continue };
            let session = match intent_to_session.get(intent_id) {
                Some(s) => s.clone(),
                None => {
                    let s = intent_log.get(&intent_id.to_string())?
                        .map(|i| i.session_id);
                    intent_to_session.insert(intent_id.into(), s.clone());
                    s
                }
            };
            let Some(session_id) = session else { continue };

            let increment = monotonic_spend(&rec.op.kind);
            if increment == 0 { continue; }
            let entry = buckets.entry(session_id).or_insert((0, 0));
            entry.0 += increment;
            entry.1 += 1;
        }

        let out: Vec<SessionBudget> = buckets
            .into_iter()
            .map(|(session_id, (spent, op_count))| SessionBudget {
                session_id,
                spent,
                op_count,
            })
            .collect();
        Ok(out)
    }
}

/// Convert an op's `budget_delta` into a monotonic spend amount.
/// `AddFunction` contributes its full `budget_cost`; modify-shape
/// ops contribute the delta only when budget *increased*.
fn monotonic_spend(kind: &lex_vcs::OperationKind) -> u64 {
    let (from, to) = kind.budget_delta();
    match (from, to) {
        (None, Some(n)) => n,
        (Some(f), Some(t)) if t > f => t - f,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_spend_handles_each_shape() {
        use lex_vcs::OperationKind;
        // AddFunction: full cost contributes.
        let k = OperationKind::AddFunction {
            sig_id: "f".into(),
            stage_id: "s".into(),
            effects: Default::default(),
            budget_cost: Some(10),
        };
        assert_eq!(monotonic_spend(&k), 10);

        // ModifyBody: only increases count.
        let k = OperationKind::ModifyBody {
            sig_id: "f".into(),
            from_stage_id: "a".into(),
            to_stage_id: "b".into(),
            from_budget: Some(10),
            to_budget: Some(15),
        };
        assert_eq!(monotonic_spend(&k), 5);

        let k = OperationKind::ModifyBody {
            sig_id: "f".into(),
            from_stage_id: "a".into(),
            to_stage_id: "b".into(),
            from_budget: Some(15),
            to_budget: Some(10),
        };
        assert_eq!(monotonic_spend(&k), 0, "decrease doesn't refund");

        // ModifyBody with no budget data on either side: zero.
        let k = OperationKind::ModifyBody {
            sig_id: "f".into(),
            from_stage_id: "a".into(),
            to_stage_id: "b".into(),
            from_budget: None,
            to_budget: None,
        };
        assert_eq!(monotonic_spend(&k), 0);
    }
}
