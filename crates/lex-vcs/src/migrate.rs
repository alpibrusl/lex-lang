//! Operation-format migration (#244).
//!
//! When [`crate::OperationFormat`] gains a new variant, every existing
//! `OpId` rotates: the canonical pre-image bytes change, so the
//! SHA-256 digest changes. A naïve in-place rewrite would also
//! invalidate every parent reference in dependent ops.
//!
//! This module computes a [`MigrationPlan`] — a topologically-ordered
//! list of [`MigrationStep`]s, one per op, each containing the new
//! `OpId` and the new [`OperationRecord`] with parents already
//! remapped — and then writes the new files in a two-phase
//! `apply_migration`: write-new, then delete-old. The intermediate
//! state has both old and new files coexisting (each consistent
//! within its own version), so a crash mid-migration leaves the
//! store readable.
//!
//! # What's in scope
//!
//! - The op log under `<root>/ops/<op_id>.json`. Parents are remapped
//!   transitively; the topological sort guarantees a parent is
//!   migrated before any child.
//!
//! # What's NOT in scope (yet)
//!
//! - **Branch heads** (`<root>/branches/<name>.json`) reference op_ids.
//!   The CLI ([`lex store migrate-ops`](../../../lex_cli/index.html))
//!   walks the branch directory after `apply_migration` and rewrites
//!   each branch's `head_op` through the returned mapping.
//! - **Attestations** carry an `op_id` field and their own `attestation_id`
//!   is computed including that op_id, so they cascade. Attestation
//!   migration is a follow-up — see the CHANGELOG entry for #244.
//! - **Intents** don't reference op_ids; ops reference intents. No
//!   action needed here.

use crate::op_log::OpLog;
use crate::operation::{OpId, Operation, OperationFormat, OperationRecord};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;

/// Describes the work needed to move an op log from one canonical
/// form to another. Built by [`plan_migration`]; consumed by
/// [`apply_migration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    pub from: OperationFormat,
    pub to: OperationFormat,
    /// Steps in topological order — every step's parents have
    /// already appeared earlier in the list, so applying in order
    /// keeps the partial DAG self-consistent.
    pub steps: Vec<MigrationStep>,
}

/// One op's migration plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStep {
    pub old_op_id: OpId,
    pub new_op_id: OpId,
    /// The [`OperationRecord`] that will replace `old_op_id`'s file.
    /// Its `op.parents` already reference the *new* op_ids; its
    /// `op_id` field equals `new_op_id`; its `format_version` equals
    /// the plan's `to` field.
    pub new_record: OperationRecord,
}

impl MigrationPlan {
    /// `true` if every step's old and new `op_id` agree — applying
    /// the plan would be a no-op. Note that `from == to` does **not**
    /// imply this: tests inject custom encoders that produce
    /// different bytes for the same source format, in which case
    /// `from == to == V1` but the migration is meaningful.
    pub fn is_no_op(&self) -> bool {
        self.steps.iter().all(|s| s.old_op_id == s.new_op_id)
    }

    /// Old → new op_id mapping in deterministic order.
    pub fn mapping(&self) -> BTreeMap<OpId, OpId> {
        self.steps
            .iter()
            .map(|s| (s.old_op_id.clone(), s.new_op_id.clone()))
            .collect()
    }
}

/// Plan a migration to `target`, using the encoder paired with that
/// format. Production callers want this; tests that need to inject a
/// different encoder (to simulate a future variant without adding it
/// to the production enum) should use [`plan_migration_with_encoder`].
pub fn plan_migration(log: &OpLog, target: OperationFormat) -> io::Result<MigrationPlan> {
    plan_migration_with_encoder(log, target, |op| op.canonical_bytes_in(target))
}

/// Plan a migration with a custom canonical encoder. Used by the
/// conformance test in `tests/migrate.rs` to simulate a hypothetical
/// V2 by adding a synthetic suffix to V1's pre-image, without
/// requiring a placeholder variant in production code.
pub fn plan_migration_with_encoder<F>(
    log: &OpLog,
    target: OperationFormat,
    encoder: F,
) -> io::Result<MigrationPlan>
where
    F: Fn(&Operation) -> Vec<u8>,
{
    let all = log.list_all()?;
    let mut by_id: BTreeMap<OpId, OperationRecord> = BTreeMap::new();
    for rec in all {
        by_id.insert(rec.op_id.clone(), rec);
    }
    let topo = topological_sort(&by_id)?;
    let from = detect_from_format(&by_id);

    let mut mapping: BTreeMap<OpId, OpId> = BTreeMap::new();
    let mut steps = Vec::with_capacity(topo.len());
    for old_id in topo {
        let rec = by_id
            .get(&old_id)
            .ok_or_else(|| io::Error::other(format!("op {old_id} disappeared during migration")))?
            .clone();
        let remapped_parents: Vec<OpId> = rec
            .op
            .parents
            .iter()
            .map(|p| {
                mapping
                    .get(p)
                    .cloned()
                    // A parent without a mapping is a parent we
                    // didn't see during list_all — treat as a
                    // dangling reference and preserve the original
                    // string. The apply step will surface this as a
                    // broken DAG when it tries to chase the missing
                    // op file.
                    .unwrap_or_else(|| p.clone())
            })
            .collect();
        let new_op = Operation {
            kind: rec.op.kind.clone(),
            parents: remapped_parents,
            intent_id: rec.op.intent_id.clone(),
        };
        let new_bytes = encoder(&new_op);
        let new_op_id = crate::canonical::hash_bytes(&new_bytes);
        let new_record = OperationRecord {
            op_id: new_op_id.clone(),
            format_version: target,
            op: new_op,
            produces: rec.produces.clone(),
        };
        mapping.insert(old_id.clone(), new_op_id.clone());
        steps.push(MigrationStep {
            old_op_id: old_id,
            new_op_id,
            new_record,
        });
    }

    Ok(MigrationPlan {
        from,
        to: target,
        steps,
    })
}

/// Apply a [`MigrationPlan`] to a live op log. Two-phase:
///
/// 1. Write every new `<new_op_id>.json` file. Idempotent — pre-
///    existing files (including the rare case where new == old) are
///    no-ops.
/// 2. Delete every old `<old_op_id>.json` file whose new id is
///    different.
///
/// On crash between phases the store is double-sized but readable;
/// re-running the plan converges. **Branch heads and attestations
/// are not rewritten by this function** — see module docs.
pub fn apply_migration(log: &OpLog, plan: &MigrationPlan) -> io::Result<()> {
    if plan.is_no_op() {
        return Ok(());
    }

    // Phase 1: write all new records.
    for step in &plan.steps {
        log.put(&step.new_record)?;
    }

    // Phase 2: delete old files whose ids changed. We collect the
    // set of new ids first so we never delete a file that's also
    // a new file (the `new == old` case).
    let new_ids: BTreeSet<&OpId> = plan.steps.iter().map(|s| &s.new_op_id).collect();
    for step in &plan.steps {
        if step.old_op_id != step.new_op_id && !new_ids.contains(&step.old_op_id) {
            log.delete(&step.old_op_id)?;
        }
    }

    Ok(())
}

/// Topological sort of the op DAG (parents before children). Stable
/// across runs because we process nodes in `BTreeMap` iteration
/// order — sorted by `op_id` — which matches the deterministic
/// canonical-form requirement.
fn topological_sort(by_id: &BTreeMap<OpId, OperationRecord>) -> io::Result<Vec<OpId>> {
    let mut indegree: BTreeMap<&OpId, usize> = BTreeMap::new();
    for id in by_id.keys() {
        indegree.insert(id, 0);
    }
    for rec in by_id.values() {
        for parent in &rec.op.parents {
            // Only count parents that are present in the log; a
            // missing parent is a dangling reference, not a cycle.
            if by_id.contains_key(parent) {
                *indegree.entry(&rec.op_id).or_insert(0) += 1;
            }
        }
    }

    // Kahn: start with all zero-indegree nodes, process in BTreeMap
    // order (which is sorted). Each processed node decrements its
    // children's indegrees.
    let mut queue: VecDeque<OpId> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| (*id).clone())
        .collect();
    let mut out = Vec::with_capacity(by_id.len());
    while let Some(id) = queue.pop_front() {
        out.push(id.clone());
        // Find children of `id` — ops whose parents include `id`.
        // BTreeMap iteration is sorted, so children-discovery order
        // is deterministic.
        for (child_id, child_rec) in by_id {
            if child_rec.op.parents.contains(&id) {
                let d = indegree.get_mut(child_id).expect("indegree present");
                *d -= 1;
                if *d == 0 {
                    queue.push_back(child_id.clone());
                }
            }
        }
    }

    if out.len() != by_id.len() {
        return Err(io::Error::other(format!(
            "op log has a cycle or unreachable component: {} of {} ops topologically sortable",
            out.len(),
            by_id.len(),
        )));
    }
    Ok(out)
}

/// Best-effort guess at the source format. If every record carries
/// V1 (or is missing the field — `serde(default)` deserializes to
/// V1), the answer is V1. A mixed log (which today shouldn't happen)
/// surfaces the most-common version; future variants will need a
/// finer answer when partial migrations exist.
fn detect_from_format(by_id: &BTreeMap<OpId, OperationRecord>) -> OperationFormat {
    let mut counts: BTreeMap<OperationFormat, usize> = BTreeMap::new();
    for rec in by_id.values() {
        *counts.entry(rec.format_version).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(f, _)| f)
        .unwrap_or(OperationFormat::V1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn add_op(parent: Option<&OpId>, sig: &str, stg: &str) -> OperationRecord {
        let parents: Vec<OpId> = parent.cloned().into_iter().collect();
        OperationRecord::new(
            Operation::new(
                OperationKind::AddFunction {
                    sig_id: sig.into(),
                    stage_id: stg.into(),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                },
                parents,
            ),
            StageTransition::Create {
                sig_id: sig.into(),
                stage_id: stg.into(),
            },
        )
    }

    #[test]
    fn migration_to_same_format_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op(None, "fac", "s0");
        log.put(&a).unwrap();
        let b = add_op(Some(&a.op_id), "fac2", "s1");
        log.put(&b).unwrap();

        let plan = plan_migration(&log, OperationFormat::V1).unwrap();
        assert_eq!(plan.from, OperationFormat::V1);
        assert_eq!(plan.to, OperationFormat::V1);
        assert!(plan.is_no_op());
        for step in &plan.steps {
            assert_eq!(step.old_op_id, step.new_op_id);
        }
        // apply_migration on a no-op leaves the log untouched.
        apply_migration(&log, &plan).unwrap();
        assert!(log.get(&a.op_id).unwrap().is_some());
        assert!(log.get(&b.op_id).unwrap().is_some());
    }

    #[test]
    fn topological_sort_orders_parents_before_children() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op(None, "fac", "s0");
        log.put(&a).unwrap();
        let b = add_op(Some(&a.op_id), "fac2", "s1");
        log.put(&b).unwrap();
        let c = add_op(Some(&b.op_id), "fac3", "s2");
        log.put(&c).unwrap();

        let plan = plan_migration(&log, OperationFormat::V1).unwrap();
        let order: Vec<_> = plan.steps.iter().map(|s| s.old_op_id.as_str()).collect();
        let pos = |id: &str| order.iter().position(|x| *x == id).unwrap();
        assert!(pos(&a.op_id) < pos(&b.op_id));
        assert!(pos(&b.op_id) < pos(&c.op_id));
    }

    #[test]
    fn empty_log_yields_empty_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let plan = plan_migration(&log, OperationFormat::V1).unwrap();
        assert!(plan.steps.is_empty());
        assert!(plan.is_no_op());
    }
}
