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

// ── Blob GC (#1007 §3 / PR 7) ────────────────────────────────────────────
//
// Mark-and-sweep over the blob space, independent of (but built on top
// of) op GC:
//
// 1. **Mark**: every blob reachable from a retained op's `SetFiles`
//    manifest, PLUS every blob bound under `blobrefs/**` (locks, loom
//    artifacts — anything a namespace ref points at is live by
//    definition, whether or not any op mentions it).
// 2. **Sweep**: any blob NOT marked, and older than a grace period
//    (default 24h) — never a blob younger than that. The grace period
//    exists because push order is stages/intents → **blobs** →
//    locks/issues → **ops** → head (#1007 §4): a blob is uploaded before
//    the op that references it, so immediately after an upload there is
//    a window where the blob exists but no op names it yet. A GC that
//    ran in that window, with no grace period, would delete a blob out
//    from under an in-flight push. 24h comfortably exceeds any
//    realistic gap between a blob upload and the op batch that follows
//    it.
//
// "Retained op" reuses exactly [`Store::plan_gc`]'s definition (branch
// reachability + predicate retention + parent-of-retained closure) —
// blob liveness must never be a stricter notion than op liveness, or
// GC could delete a blob whose `SetFiles` op survives, leaving that op's
// manifest referencing an object nobody can fetch again (#1007 always-
// valid-HEAD would then fail retroactively for a head no one touched).

/// The plan for a single blob-GC pass: which blobs are still referenced
/// (`live`), which are slated for deletion, and which would be
/// unreferenced but are still inside the grace period (kept for
/// visibility — surfaced by `lex op gc --blobs --dry-run`, not acted on).
#[derive(Debug, Clone)]
pub struct BlobGcPlan {
    pub live: BTreeSet<crate::files::BlobId>,
    pub to_delete: Vec<crate::files::BlobId>,
    pub skipped_within_grace: Vec<crate::files::BlobId>,
}

impl BlobGcPlan {
    /// True when there's nothing to delete.
    pub fn is_empty(&self) -> bool {
        self.to_delete.is_empty()
    }
}

impl Store {
    /// Build a [`BlobGcPlan`]: mark every blob reachable from a retained
    /// `SetFiles` op's manifest plus every `blobrefs/**` binding, then
    /// sweep everything else older than `grace`. See the module docs
    /// above for why the grace period exists and why "retained" mirrors
    /// [`Self::plan_gc`] rather than recomputing branch reachability
    /// independently.
    pub fn plan_blob_gc(&self, grace: std::time::Duration) -> Result<BlobGcPlan, StoreError> {
        let op_plan = self.plan_gc(&[])?;
        let log = OpLog::open(self.root())?;

        let mut live: BTreeSet<String> = BTreeSet::new();
        for op_id in op_plan.retained.keys() {
            let Some(rec) = log.get(op_id)? else { continue };
            if let lex_vcs::OperationKind::SetFiles { manifest } = &rec.op.kind {
                live.insert(manifest.clone());
                // A manifest that fails to load (corrupt, or a blob
                // already lost) contributes only its own id above —
                // there's nothing else to mark, and GC must not error
                // out of a whole pass over one bad manifest.
                if let Ok(m) = self.get_manifest(manifest) {
                    for e in m.entries.values() {
                        live.insert(e.blob.clone());
                    }
                }
            }
        }
        for sha in self.all_blob_ref_shas()? {
            live.insert(sha);
        }

        let now = std::time::SystemTime::now();
        let mut to_delete = Vec::new();
        let mut skipped_within_grace = Vec::new();
        for (id, mtime) in self.list_blob_ids_with_mtime()? {
            if live.contains(&id) {
                continue;
            }
            match now.duration_since(mtime) {
                Ok(age) if age >= grace => to_delete.push(id),
                // `Ok(age) if age < grace` (too young) or `Err` (mtime in
                // the future — a clock skew we should never race ahead
                // of) both mean "not provably safe to delete yet".
                _ => skipped_within_grace.push(id),
            }
        }
        to_delete.sort();
        skipped_within_grace.sort();
        Ok(BlobGcPlan { live, to_delete, skipped_within_grace })
    }

    /// Apply a [`BlobGcPlan`] — delete every blob in `plan.to_delete`.
    /// Idempotent: a blob already gone (deleted by a concurrent GC pass,
    /// e.g. another replica) is not an error. Returns the number of
    /// blobs actually removed.
    pub fn apply_blob_gc(&self, plan: &BlobGcPlan) -> Result<usize, StoreError> {
        let mut removed = 0usize;
        for id in &plan.to_delete {
            self.delete_blob(id)?;
            removed += 1;
        }
        Ok(removed)
    }
}
