//! `Store::apply_operation` — the only way to advance a branch
//! head's op (post-#129).

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

#[test]
fn apply_operation_advances_head_on_main() {
    let (s, _tmp) = fresh();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let t = StageTransition::Create {
        sig_id: "fac".into(),
        stage_id: "stg-1".into(),
    };
    let op_id = s.apply_operation(DEFAULT_BRANCH, op, t).unwrap();
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-1".to_string()));

    // The branch file now exists with this head_op.
    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(b.head_op.as_deref(), Some(op_id.as_str()));
}

#[test]
fn apply_operation_chains_against_existing_head() {
    let (s, _tmp) = fresh();
    let op1 = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let op_id1 = s.apply_operation(DEFAULT_BRANCH, op1, StageTransition::Create {
        sig_id: "fac".into(), stage_id: "stg-1".into(),
    }).unwrap();
    let op2 = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
        },
        [op_id1.clone()],
    );
    let op_id2 = s.apply_operation(DEFAULT_BRANCH, op2, StageTransition::Replace {
        sig_id: "fac".into(), from: "stg-1".into(), to: "stg-2".into(),
    }).unwrap();
    assert_ne!(op_id1, op_id2);
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-2".to_string()));
}

#[test]
fn apply_operation_with_stale_parent_errors() {
    let (s, _tmp) = fresh();
    let op1 = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    s.apply_operation(DEFAULT_BRANCH, op1, StageTransition::Create {
        sig_id: "fac".into(), stage_id: "stg-1".into(),
    }).unwrap();
    // Op claims a different parent than the current head.
    let bogus = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
        },
        ["someone-else".into()],
    );
    let err = s.apply_operation(DEFAULT_BRANCH, bogus, StageTransition::Replace {
        sig_id: "fac".into(), from: "stg-1".into(), to: "stg-2".into(),
    });
    assert!(err.is_err(), "expected stale-parent rejection");
    // Head is unchanged.
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-1".to_string()));
}
