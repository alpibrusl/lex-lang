//! The apply gate. Validates an operation's parents against a known
//! branch head, then persists it via [`OpLog`]. Issue #129 keeps this
//! narrow: no type checking, no effect verification — those are #130.

use crate::op_log::OpLog;
use crate::operation::{OpId, Operation, OperationRecord, StageTransition};
use std::io;

#[derive(Debug)]
pub struct NewHead {
    pub op_id: OpId,
    pub record: OperationRecord,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("stale parent: branch head is {expected:?} but op's parents are {op_parents:?}")]
    StaleParent {
        expected: Option<OpId>,
        op_parents: Vec<OpId>,
    },
    #[error("merge op references unknown second parent {0}")]
    UnknownMergeParent(OpId),
    #[error(transparent)]
    Persist(#[from] io::Error),
}

/// Apply an operation against a branch head and persist it.
///
/// Validates parents:
/// - If `op.parents.is_empty()`: `head_op` must be `None` (genesis op
///   on an empty branch).
/// - If `op.parents.len() == 1`: that parent must equal `head_op`.
/// - If `op.parents.len() == 2`: one parent must equal `head_op`, and
///   the other must already exist in the log (a merge op's
///   second-parent ancestry must be reachable).
/// - All other arities are rejected as `StaleParent`.
pub fn apply(
    op_log: &OpLog,
    head_op: Option<&OpId>,
    op: Operation,
    transition: StageTransition,
) -> Result<NewHead, ApplyError> {
    match (op.parents.len(), head_op) {
        (0, None) => {}
        (1, Some(h)) if op.parents[0] == *h => {}
        (2, Some(h)) => {
            if op.parents[0] != *h && op.parents[1] != *h {
                return Err(ApplyError::StaleParent {
                    expected: head_op.cloned(),
                    op_parents: op.parents.clone(),
                });
            }
            // The non-head parent must exist in the log.
            let other = if op.parents[0] == *h { &op.parents[1] } else { &op.parents[0] };
            if op_log.get(other)?.is_none() {
                return Err(ApplyError::UnknownMergeParent(other.clone()));
            }
        }
        _ => {
            return Err(ApplyError::StaleParent {
                expected: head_op.cloned(),
                op_parents: op.parents.clone(),
            });
        }
    }

    let record = OperationRecord::new(op, transition);
    op_log.put(&record)?;
    Ok(NewHead { op_id: record.op_id.clone(), record })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn add_fac() -> (Operation, StageTransition) {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac".into(),
                stage_id: "s1".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        let t = StageTransition::Create {
            sig_id: "fac".into(),
            stage_id: "s1".into(),
        };
        (op, t)
    }

    #[test]
    fn parentless_op_against_empty_head_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op, t) = add_fac();
        let head = apply(&log, None, op, t).unwrap();
        assert!(log.get(&head.op_id).unwrap().is_some());
    }

    #[test]
    fn parentless_op_against_non_empty_head_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op1, t1) = add_fac();
        let head1 = apply(&log, None, op1, t1).unwrap();
        let (op2, t2) = add_fac(); // parentless again
        let err = apply(&log, Some(&head1.op_id), op2, t2).unwrap_err();
        assert!(matches!(err, ApplyError::StaleParent { .. }));
    }

    #[test]
    fn single_parent_matching_head_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op1, t1) = add_fac();
        let head1 = apply(&log, None, op1, t1).unwrap();
        let modify = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fac".into(),
                from_stage_id: "s1".into(),
                to_stage_id: "s2".into(),
            },
            [head1.op_id.clone()],
        );
        let t = StageTransition::Replace {
            sig_id: "fac".into(),
            from: "s1".into(),
            to: "s2".into(),
        };
        let head2 = apply(&log, Some(&head1.op_id), modify, t).unwrap();
        assert_ne!(head2.op_id, head1.op_id);
    }

    #[test]
    fn single_parent_not_matching_head_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op1, t1) = add_fac();
        let head1 = apply(&log, None, op1, t1).unwrap();
        // op claims a different parent than head.
        let bogus = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fac".into(),
                from_stage_id: "s1".into(),
                to_stage_id: "s2".into(),
            },
            ["someone-else".into()],
        );
        let t = StageTransition::Replace {
            sig_id: "fac".into(),
            from: "s1".into(),
            to: "s2".into(),
        };
        let err = apply(&log, Some(&head1.op_id), bogus, t).unwrap_err();
        assert!(matches!(err, ApplyError::StaleParent { .. }));
    }

    #[test]
    fn merge_op_with_known_second_parent_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op_a, t_a) = add_fac();
        let head_a = apply(&log, None, op_a, t_a).unwrap();
        let other = Operation::new(
            OperationKind::AddFunction {
                sig_id: "double".into(),
                stage_id: "d1".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        let head_b = apply(&log, None, other, StageTransition::Create {
            sig_id: "double".into(), stage_id: "d1".into(),
        }).unwrap();
        // Merge op: parents = [head_a, head_b].
        let merge = Operation::new(
            OperationKind::Merge { resolved: 1 },
            [head_a.op_id.clone(), head_b.op_id.clone()],
        );
        let t = StageTransition::Merge {
            entries: std::iter::once(("double".to_string(), Some("d1".to_string())))
                .collect(),
        };
        let merged = apply(&log, Some(&head_a.op_id), merge, t).unwrap();
        assert!(log.get(&merged.op_id).unwrap().is_some());
    }

    #[test]
    fn merge_op_with_unknown_second_parent_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op_a, t_a) = add_fac();
        let head_a = apply(&log, None, op_a, t_a).unwrap();
        let merge = Operation::new(
            OperationKind::Merge { resolved: 0 },
            [head_a.op_id.clone(), "ghost".into()],
        );
        let t = StageTransition::Merge { entries: Default::default() };
        let err = apply(&log, Some(&head_a.op_id), merge, t).unwrap_err();
        assert!(matches!(err, ApplyError::UnknownMergeParent(_)));
    }
}
