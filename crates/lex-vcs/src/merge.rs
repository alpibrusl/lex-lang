//! Op-DAG three-way merge.
//!
//! 1. Compute LCA of src and dst heads.
//! 2. Get ops on each side since the LCA.
//! 3. Group by the `SigId` they touch; classify each group.

use crate::op_log::OpLog;
use crate::operation::{OpId, OperationKind, OperationRecord, SigId, StageId};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Both sides converged on the same op_id for this sig.
    Both { sig_id: SigId, stage_id: Option<StageId> },
    /// Only src touched it.
    Src  { sig_id: SigId, stage_id: Option<StageId> },
    /// Only dst touched it.
    Dst  { sig_id: SigId, stage_id: Option<StageId> },
    /// Conflict: both sides touched it with different ops.
    Conflict {
        sig_id: SigId,
        kind: ConflictKind,
        base: Option<StageId>,
        src:  Option<StageId>,
        dst:  Option<StageId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    ModifyModify,
    ModifyDelete,
    DeleteModify,
    AddAdd,
}

#[derive(Debug)]
pub struct MergeOutput {
    pub lca: Option<OpId>,
    pub outcomes: Vec<MergeOutcome>,
}

pub fn merge(
    op_log: &OpLog,
    src_head: Option<&OpId>,
    dst_head: Option<&OpId>,
) -> io::Result<MergeOutput> {
    let lca = match (src_head, dst_head) {
        (Some(s), Some(d)) => op_log.lca(s, d)?,
        _ => None,
    };
    let src_ops = match src_head {
        Some(h) => op_log.ops_since(h, lca.as_ref())?,
        None => Vec::new(),
    };
    let dst_ops = match dst_head {
        Some(h) => op_log.ops_since(h, lca.as_ref())?,
        None => Vec::new(),
    };

    let src_by_sig = group_by_sig(&src_ops);
    let dst_by_sig = group_by_sig(&dst_ops);

    let lca_head: BTreeMap<SigId, StageId> = match lca.as_ref() {
        Some(id) => head_at(op_log, id)?,
        None => BTreeMap::new(),
    };

    let mut outcomes = Vec::new();
    let sigs: BTreeSet<&SigId> = src_by_sig.keys().chain(dst_by_sig.keys()).collect();
    for sig in sigs {
        let s = src_by_sig.get(sig);
        let d = dst_by_sig.get(sig);
        let s_stage = s.map(|recs| latest_stage(recs));
        let d_stage = d.map(|recs| latest_stage(recs));
        match (s, d) {
            (Some(s_recs), Some(d_recs)) => {
                let s_last = s_recs.last().map(|r| r.op_id.as_str()).unwrap_or("");
                let d_last = d_recs.last().map(|r| r.op_id.as_str()).unwrap_or("");
                if s_last == d_last {
                    outcomes.push(MergeOutcome::Both {
                        sig_id: sig.clone(),
                        stage_id: s_stage.unwrap(),
                    });
                } else {
                    let kind = classify(&s_stage.clone().unwrap(), &d_stage.clone().unwrap(), &lca_head, sig);
                    outcomes.push(MergeOutcome::Conflict {
                        sig_id: sig.clone(),
                        kind,
                        base: lca_head.get(sig).cloned(),
                        src:  s_stage.unwrap(),
                        dst:  d_stage.unwrap(),
                    });
                }
            }
            (Some(_), None) => {
                outcomes.push(MergeOutcome::Src {
                    sig_id: sig.clone(),
                    stage_id: s_stage.unwrap(),
                });
            }
            (None, Some(_)) => {
                outcomes.push(MergeOutcome::Dst {
                    sig_id: sig.clone(),
                    stage_id: d_stage.unwrap(),
                });
            }
            (None, None) => unreachable!(),
        }
    }

    Ok(MergeOutput { lca, outcomes })
}

fn group_by_sig(ops: &[OperationRecord]) -> BTreeMap<SigId, Vec<&OperationRecord>> {
    let mut out: BTreeMap<SigId, Vec<&OperationRecord>> = BTreeMap::new();
    for r in ops {
        if let Some(sig) = touched_sig(&r.op.kind) {
            out.entry(sig).or_default().push(r);
        }
    }
    // ops_since returned newest-first; reverse to oldest-first per sig
    // so `latest_stage` reads the right entry.
    for v in out.values_mut() { v.reverse(); }
    out
}

fn touched_sig(k: &OperationKind) -> Option<SigId> {
    match k {
        OperationKind::AddFunction { sig_id, .. }
        | OperationKind::RemoveFunction { sig_id, .. }
        | OperationKind::ModifyBody { sig_id, .. }
        | OperationKind::ChangeEffectSig { sig_id, .. }
        | OperationKind::AddType { sig_id, .. }
        | OperationKind::RemoveType { sig_id, .. }
        | OperationKind::ModifyType { sig_id, .. } => Some(sig_id.clone()),
        OperationKind::RenameSymbol { to, .. } => Some(to.clone()),
        OperationKind::AddImport { .. }
        | OperationKind::RemoveImport { .. }
        | OperationKind::Merge { .. } => None,
    }
}

/// Given a chronological (oldest-first) list of ops on a sig, return
/// the resulting stage_id (`None` if the sig was removed).
fn latest_stage(recs: &[&OperationRecord]) -> Option<StageId> {
    use crate::operation::StageTransition::*;
    let mut current: Option<StageId> = None;
    for r in recs {
        match &r.produces {
            Create { stage_id, .. } => current = Some(stage_id.clone()),
            Replace { to, .. } => current = Some(to.clone()),
            Remove { .. } => current = None,
            Rename { body_stage_id, .. } => current = Some(body_stage_id.clone()),
            ImportOnly | Merge { .. } => {}
        }
    }
    current
}

fn head_at(op_log: &OpLog, head: &OpId) -> io::Result<BTreeMap<SigId, StageId>> {
    let mut map = BTreeMap::new();
    for r in op_log.walk_forward(head, None)? {
        use crate::operation::StageTransition::*;
        match &r.produces {
            Create { sig_id, stage_id } => { map.insert(sig_id.clone(), stage_id.clone()); }
            Replace { sig_id, to, .. } => { map.insert(sig_id.clone(), to.clone()); }
            Remove { sig_id, .. } => { map.remove(sig_id); }
            Rename { from, to, body_stage_id } => {
                map.remove(from);
                map.insert(to.clone(), body_stage_id.clone());
            }
            ImportOnly => {}
            Merge { entries } => {
                for (sig, stage) in entries {
                    match stage {
                        Some(s) => { map.insert(sig.clone(), s.clone()); }
                        None    => { map.remove(sig); }
                    }
                }
            }
        }
    }
    Ok(map)
}

fn classify(
    src: &Option<StageId>,
    dst: &Option<StageId>,
    base: &BTreeMap<SigId, StageId>,
    sig: &SigId,
) -> ConflictKind {
    let in_base = base.contains_key(sig);
    match (in_base, src.is_some(), dst.is_some()) {
        (false, true, true)  => ConflictKind::AddAdd,
        (true,  true, true)  => ConflictKind::ModifyModify,
        (true,  true, false) => ConflictKind::ModifyDelete,
        (true,  false, true) => ConflictKind::DeleteModify,
        // Other combos shouldn't happen for a "both touched" group.
        _ => ConflictKind::ModifyModify,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply;
    use crate::operation::{Operation, OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn fresh() -> (OpLog, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (OpLog::open(tmp.path()).unwrap(), tmp)
    }

    fn add_fn(log: &OpLog, parent: Option<&OpId>, sig: &str, stg: &str) -> OpId {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stg.into(),
                effects: BTreeSet::new(),
            },
            parent.cloned().into_iter().collect::<Vec<_>>(),
        );
        let t = StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() };
        apply(log, parent, op, t).unwrap().op_id
    }

    fn modify_body(log: &OpLog, parent: &OpId, sig: &str, from: &str, to: &str) -> OpId {
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
            },
            [parent.clone()],
        );
        let t = StageTransition::Replace {
            sig_id: sig.into(), from: from.into(), to: to.into(),
        };
        apply(log, Some(parent), op, t).unwrap().op_id
    }

    #[test]
    fn disjoint_sigs_merge_cleanly() {
        let (log, _tmp) = fresh();
        let root = add_fn(&log, None, "shared", "s0");
        let s_only = add_fn(&log, Some(&root), "src-only", "src1");
        let d_only = add_fn(&log, Some(&root), "dst-only", "dst1");

        let out = merge(&log, Some(&s_only), Some(&d_only)).unwrap();
        assert_eq!(out.lca.as_ref(), Some(&root));
        let kinds: Vec<&str> = out.outcomes.iter().map(|o| match o {
            MergeOutcome::Src { .. } => "src",
            MergeOutcome::Dst { .. } => "dst",
            MergeOutcome::Both { .. } => "both",
            MergeOutcome::Conflict { .. } => "conflict",
        }).collect();
        assert!(kinds.contains(&"src") && kinds.contains(&"dst"));
        assert!(!kinds.contains(&"conflict"));
    }

    #[test]
    fn same_sig_divergent_is_modify_modify_conflict() {
        let (log, _tmp) = fresh();
        let root = add_fn(&log, None, "fac", "s0");
        let src  = modify_body(&log, &root, "fac", "s0", "s-src");
        let dst  = modify_body(&log, &root, "fac", "s0", "s-dst");

        let out = merge(&log, Some(&src), Some(&dst)).unwrap();
        let conflict = out.outcomes.iter().find(|o| matches!(o, MergeOutcome::Conflict { .. }));
        assert!(conflict.is_some());
        if let Some(MergeOutcome::Conflict { kind, .. }) = conflict {
            assert!(matches!(kind, ConflictKind::ModifyModify));
        }
    }

    #[test]
    fn independent_histories_no_lca() {
        let (log, _tmp) = fresh();
        let a = add_fn(&log, None, "a", "sa");
        let b = add_fn(&log, None, "b", "sb");
        let out = merge(&log, Some(&a), Some(&b)).unwrap();
        assert!(out.lca.is_none());
    }
}
