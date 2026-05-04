//! Per-op write-time gate (#130 cont'd).
//! `Store::apply_operation_checked` is the single-op variant of
//! the publish-program gate: caller passes the candidate program;
//! gate runs `lex_types::check_program` first; on rejection the
//! branch head is unchanged and no op record is persisted.

use lex_ast::canonicalize_program;
use lex_store::{Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    let prog = parse_source(src).expect("parse");
    canonicalize_program(&prog)
}

fn add_fac_op() -> (Operation, StageTransition) {
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
    (op, t)
}

#[test]
fn apply_operation_checked_advances_head_on_clean_candidate() {
    let (s, _tmp) = fresh();
    let candidate = parse(
        "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n",
    );
    let (op, t) = add_fac_op();
    let op_id = s
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect("clean candidate should pass the gate");
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-1".to_string()));
    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(b.head_op.as_deref(), Some(op_id.as_str()));
}

#[test]
fn apply_operation_checked_rejects_type_broken_candidate() {
    let (s, _tmp) = fresh();
    // The candidate references `not_defined` — type checker emits
    // an `UnknownIdentifier`. Gate runs before any side effect.
    let candidate = parse("fn broken(x :: Int) -> Int { not_defined(x) }\n");
    let (op, t) = add_fac_op();

    let err = s
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect_err("expected TypeError");
    assert!(matches!(err, StoreError::TypeError(_)));

    // Branch head unchanged.
    assert!(
        s.get_branch(DEFAULT_BRANCH).unwrap().is_none(),
        "branch should not have been created on the rejection path",
    );

    // No op records persisted on the failure path.
    let ops_dir = s.root().join("ops");
    if ops_dir.exists() {
        let count = std::fs::read_dir(&ops_dir).unwrap().count();
        assert_eq!(count, 0, "no op records should be persisted on TypeError");
    }
}

#[test]
fn apply_operation_checked_does_not_advance_head_after_rejection() {
    // The earlier test confirms the branch isn't created on rejection
    // against a fresh store. This one confirms the same behavior
    // against an existing branch with prior history — the gate
    // must not advance the head even when the branch already
    // exists.
    let (s, _tmp) = fresh();

    // First op lands cleanly to give the branch a head_op.
    let initial_candidate = parse(
        "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n",
    );
    let (op1, t1) = add_fac_op();
    let head1 = s
        .apply_operation_checked(DEFAULT_BRANCH, op1, t1, &initial_candidate)
        .expect("first op should land");

    // Second op submitted with a type-broken candidate. Should
    // be rejected; branch head must still point at head1.
    let bad_candidate = parse("fn broken(x :: Int) -> Int { not_defined(x) }\n");
    let op2 = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
        },
        [head1.clone()],
    );
    let t2 = StageTransition::Replace {
        sig_id: "fac".into(),
        from: "stg-1".into(),
        to: "stg-2".into(),
    };
    let err = s
        .apply_operation_checked(DEFAULT_BRANCH, op2, t2, &bad_candidate)
        .expect_err("expected TypeError");
    assert!(matches!(err, StoreError::TypeError(_)));

    let b_after = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(
        b_after.head_op.as_deref(),
        Some(head1.as_str()),
        "branch head should still point at head1 after rejected op",
    );
}

#[test]
fn apply_operation_checked_propagates_apply_errors() {
    // Type-clean candidate but stale parent — gate accepts the
    // typecheck, then the underlying `apply_operation` rejects on
    // structure. The error should pass through as
    // `StoreError::Apply(StaleParent)`, not `TypeError`.
    let (s, _tmp) = fresh();
    let candidate = parse(
        "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n",
    );
    // First op lands.
    let (op1, t1) = add_fac_op();
    let _ = s
        .apply_operation_checked(DEFAULT_BRANCH, op1, t1, &candidate)
        .unwrap();

    // Second op declares the wrong parent.
    let bogus = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
        },
        ["someone-else".into()],
    );
    let t = StageTransition::Replace {
        sig_id: "fac".into(),
        from: "stg-1".into(),
        to: "stg-2".into(),
    };
    let err = s
        .apply_operation_checked(DEFAULT_BRANCH, bogus, t, &candidate)
        .expect_err("expected Apply(StaleParent)");
    match err {
        StoreError::Apply(_) => {}
        other => panic!("expected StoreError::Apply, got {other:?}"),
    }
}
