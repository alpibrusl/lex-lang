//! Branch tests over the op-DAG model.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn add(s: &Store, branch: &str, sig: &str, stg: &str) -> String {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stg.into(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect::<Vec<_>>(),
    );
    let t = StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() };
    s.apply_operation(branch, op, t).unwrap()
}

fn modify(s: &Store, branch: &str, sig: &str, from: &str, to: &str) -> String {
    let parent = s.get_branch(branch).unwrap().and_then(|b| b.head_op).unwrap();
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.into(),
            from_stage_id: from.into(),
            to_stage_id: to.into(),
            from_budget: None,
            to_budget: None,
        },
        [parent],
    );
    let t = StageTransition::Replace {
        sig_id: sig.into(), from: from.into(), to: to.into(),
    };
    s.apply_operation(branch, op, t).unwrap()
}

#[test]
fn fresh_store_lists_only_main() {
    let (s, _tmp) = fresh();
    assert_eq!(s.list_branches().unwrap(), vec![DEFAULT_BRANCH.to_string()]);
    assert_eq!(s.current_branch(), DEFAULT_BRANCH);
}

#[test]
fn create_branch_inherits_head_op() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature-x", DEFAULT_BRANCH).unwrap();
    assert_eq!(
        s.branch_head("feature-x").unwrap().get("sig1"),
        Some(&"stageA".to_string()),
    );
}

#[test]
fn merge_clean_when_only_one_side_modifies() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, "feature", "sig1", "stageA", "stageB");
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 0, "report: {report:?}");
    assert_eq!(report.merged.len(), 1);
    assert_eq!(report.merged[0].stage_id, "stageB");
}

#[test]
fn merge_conflict_when_both_sides_modify_same_sig() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, DEFAULT_BRANCH, "sig1", "stageA", "stageB");
    let _ = modify(&s, "feature",      "sig1", "stageA", "stageC");
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, "modify-modify");
}

#[test]
fn commit_merge_advances_dst_head_op() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, "feature", "sig1", "stageA", "stageB");
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    s.commit_merge(DEFAULT_BRANCH, &report).unwrap();
    assert_eq!(
        s.branch_head(DEFAULT_BRANCH).unwrap().get("sig1"),
        Some(&"stageB".to_string()),
    );
    assert_eq!(s.branch_log(DEFAULT_BRANCH).unwrap().len(), 1);
}

#[test]
fn delete_branch_refused_when_current_or_default() {
    let (s, _tmp) = fresh();
    s.create_branch("foo", DEFAULT_BRANCH).unwrap();
    s.set_current_branch("foo").unwrap();
    assert!(s.delete_branch("foo").is_err());
    assert!(s.delete_branch(DEFAULT_BRANCH).is_err());
    s.set_current_branch(DEFAULT_BRANCH).unwrap();
    s.delete_branch("foo").unwrap();
    assert_eq!(s.list_branches().unwrap(), vec![DEFAULT_BRANCH.to_string()]);
}

fn remove(s: &Store, branch: &str, sig: &str, last: &str) -> String {
    let parent = s.get_branch(branch).unwrap().and_then(|b| b.head_op).unwrap();
    let op = Operation::new(
        OperationKind::RemoveFunction {
            sig_id: sig.into(),
            last_stage_id: last.into(),
        },
        [parent],
    );
    let t = StageTransition::Remove { sig_id: sig.into(), last: last.into() };
    s.apply_operation(branch, op, t).unwrap()
}

#[test]
fn merge_into_empty_dst_fast_forwards() {
    let (s, _tmp) = fresh();
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = add(&s, "feature", "sig1", "stageA");
    // main is empty (no head_op). Merge feature → main should fast-forward.
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    s.commit_merge(DEFAULT_BRANCH, &report).unwrap();
    assert_eq!(
        s.branch_head(DEFAULT_BRANCH).unwrap().get("sig1"),
        Some(&"stageA".to_string()),
    );
    let head_op = s.get_branch(DEFAULT_BRANCH).unwrap().and_then(|b| b.head_op);
    assert!(head_op.is_some(), "main should now have a head_op");
}

#[test]
fn merge_modify_delete_conflict() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, "feature", "sig1", "stageA", "stageB"); // src modifies
    let _ = remove(&s, DEFAULT_BRANCH, "sig1", "stageA");      // dst deletes
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, "modify-delete");
}

#[test]
fn merge_delete_modify_conflict() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = remove(&s, "feature", "sig1", "stageA");           // src deletes
    let _ = modify(&s, DEFAULT_BRANCH, "sig1", "stageA", "stageB"); // dst modifies
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, "delete-modify");
}

#[test]
fn merge_add_add_conflict() {
    let (s, _tmp) = fresh();
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = add(&s, "feature", "newsig", "stageA");           // src adds
    let _ = add(&s, DEFAULT_BRANCH, "newsig", "stageB");      // dst adds different stage
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, "add-add");
}
