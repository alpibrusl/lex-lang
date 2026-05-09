//! Predicate-driven garbage collection of the op log (#261 slice 2).
//!
//! Three retention rules combine to form the surviving set:
//!
//! 1. **Branch reachability** — every op reachable from any branch
//!    head's `head_op` is retained. Always on; not configurable.
//!    The branch DAG is the source of truth; deleting an op
//!    referenced by a branch head would corrupt history.
//! 2. **Predicate match** — `policy.gc_retention.retain` lists
//!    [`lex_vcs::Predicate`]s; ops matching any one are retained.
//!    Useful for "keep every op produced under session X" or
//!    "keep all `EffectAudit`-tagged ops" (when those predicates
//!    land).
//! 3. **Parent-of-retained closure** — if op X is retained, every
//!    parent of X is retained too. Walks transitively up the DAG.
//!    This honors the acceptance criterion "Refuse to delete an op
//!    that's still a parent of a retained op."
//!
//! Apply is idempotent: re-running on a store that's already been
//! GC'd has no further effect because the surviving set is stable.

use crate::policy::PolicyFile;
use crate::store::{Store, StoreError};
use lex_vcs::{OpId, OpLog, Predicate};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Why an op survived a GC plan. Serialized as JSON in the
/// `lex op gc --dry-run` envelope so reviewers can see the
/// reasoning per op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionReason {
    /// Reachable via DAG walk from at least one branch head.
    ReachableFromBranch,
    /// Matched at least one `policy.gc_retention.retain` predicate
    /// (the index is into that list, not into the merged input —
    /// CLI overrides land before policy entries).
    MatchedPredicate(usize),
    /// Ancestor of an op retained by one of the above rules.
    /// Closure rule preserving DAG integrity.
    ParentOfRetained,
}

/// The plan for a single GC pass: which ops survive (with the
/// reason) and which are slated for deletion. `apply_gc(plan)`
/// turns this into actual filesystem changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GcPlan {
    pub retained: BTreeMap<OpId, RetentionReason>,
    pub to_delete: Vec<OpId>,
}

impl GcPlan {
    /// True when there's nothing to delete — the common case for a
    /// fresh store or a re-run after a previous GC pass.
    pub fn is_empty(&self) -> bool {
        self.to_delete.is_empty()
    }
}

impl Store {
    /// Build a [`GcPlan`] from the store's current state plus an
    /// optional list of additional retention predicates from the
    /// CLI (`lex op gc --retain ...`). The policy file's
    /// `gc_retention.retain` entries are appended to those.
    ///
    /// Returns `StoreError::Io(InvalidData, ...)` if a predicate
    /// in `policy.json` fails to parse.
    pub fn plan_gc(
        &self,
        cli_retain: &[Predicate],
    ) -> Result<GcPlan, StoreError> {
        let log = OpLog::open(self.root())?;
        // 1. Collect every op currently in the log. This is the
        //    universe we'll partition into retained vs to_delete.
        let universe: BTreeSet<OpId> = log
            .list_all()?
            .into_iter()
            .map(|r| r.op_id)
            .collect();

        let mut retained: BTreeMap<OpId, RetentionReason> = BTreeMap::new();

        // 2. Branch reachability. Walk every branch head; mark
        //    every op in any walk-back as ReachableFromBranch.
        for branch_name in self.list_branches()? {
            let Some(branch) = self.get_branch(&branch_name)? else { continue };
            let Some(head) = branch.head_op else { continue };
            for rec in log.walk_back(&head, None)? {
                retained
                    .entry(rec.op_id)
                    .or_insert(RetentionReason::ReachableFromBranch);
            }
        }

        // 3. Predicate-based retention. CLI retain predicates first
        //    (their indices start at 0), then policy.json entries
        //    (their indices continue).
        let mut all_retain: Vec<Predicate> = cli_retain.to_vec();
        let policy = PolicyFile::load_optional(self.root())?;
        for (i, raw) in policy.gc_retention.retain.iter().enumerate() {
            let pred = Predicate::from_value(raw)
                .map_err(|e| StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("policy.gc_retention.retain[{i}]: {e}"),
                )))?;
            all_retain.push(pred);
        }
        for (i, predicate) in all_retain.iter().enumerate() {
            for rec in lex_vcs::evaluate(&log, predicate)? {
                retained
                    .entry(rec.op_id)
                    .or_insert(RetentionReason::MatchedPredicate(i));
            }
        }

        // 4. Parent-of-retained closure. Walk every retained op's
        //    parents transitively; any not yet retained gets the
        //    ParentOfRetained reason.
        let frontier: Vec<OpId> = retained.keys().cloned().collect();
        for op_id in frontier {
            for rec in log.walk_back(&op_id, None)? {
                retained
                    .entry(rec.op_id)
                    .or_insert(RetentionReason::ParentOfRetained);
            }
        }

        // 5. The deletion set is the universe minus the retained.
        let to_delete: Vec<OpId> = universe
            .iter()
            .filter(|id| !retained.contains_key(*id))
            .cloned()
            .collect();

        Ok(GcPlan { retained, to_delete })
    }

    /// Apply a [`GcPlan`] — actually delete every op in
    /// `plan.to_delete`. Idempotent: running again on the same
    /// store after a successful apply yields a plan with an empty
    /// deletion set.
    ///
    /// Returns the number of op records actually removed (loose
    /// files deleted + packed ops dropped during pack rewrites).
    pub fn apply_gc(&self, plan: &GcPlan) -> Result<usize, StoreError> {
        if plan.to_delete.is_empty() {
            return Ok(0);
        }
        let log = OpLog::open(self.root())?;
        let victims: BTreeSet<OpId> = plan.to_delete.iter().cloned().collect();
        Ok(log.evict(&victims)?)
    }
}

impl PolicyFile {
    /// Convenience: load policy.json or return the default. Used
    /// by [`Store::plan_gc`] which doesn't care whether the file
    /// exists — absent file ↔ empty policy ↔ no retention rules.
    fn load_optional(root: &std::path::Path) -> std::io::Result<Self> {
        Ok(crate::policy::load(root)?.unwrap_or_default())
    }
}
