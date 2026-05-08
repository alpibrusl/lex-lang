//! Conformance tests for #257: Trace attestations with `op_id`
//! populated for ops committed during a traced run.
//!
//! Pre-#257, `AttestationKind::Trace` carried `run_id` and
//! `root_target` but no `op_id` — `lex trace --op <op_id>` had a
//! filter wired but no producer to surface. #257 closes that loop:
//! `Store::record_op_trace` and
//! `Store::record_run_committed_ops_since` emit per-stage Trace
//! attestations with `op_id: Some(...)` set.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "test-runner".into(),
        version: "0.0.0".into(),
        model: None,
    }
}

fn make_add_op(sig: &str, stage: &str) -> (Operation, StageTransition) {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stage.into(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        [],
    );
    let t = StageTransition::Create {
        sig_id: sig.into(),
        stage_id: stage.into(),
    };
    (op, t)
}

#[test]
fn record_op_trace_emits_per_stage_attestation_with_op_id() {
    let (s, _tmp) = fresh();
    let (op, t) = make_add_op("fac", "stg-1");
    let op_id = s.apply_operation(DEFAULT_BRANCH, op, t).unwrap();

    let n = s.record_op_trace(
        "run-abc",
        "fac",
        &op_id,
        lex_vcs::AttestationResult::Passed,
        producer(),
    ).unwrap();
    assert_eq!(n, 1, "AddFunction produces exactly one attestable stage");

    let attlog = s.attestation_log().unwrap();
    let by_run = attlog.list_for_run(&"run-abc".to_string()).unwrap();
    assert_eq!(by_run.len(), 1);
    let att = &by_run[0];
    assert_eq!(att.op_id.as_deref(), Some(op_id.as_str()),
        "Trace attestation must carry the committed op_id");
    match &att.kind {
        lex_vcs::AttestationKind::Trace { run_id, root_target } => {
            assert_eq!(run_id, "run-abc");
            assert_eq!(root_target, "fac");
        }
        other => panic!("expected Trace kind, got {other:?}"),
    }
}

#[test]
fn record_op_trace_is_idempotent() {
    // Re-emitting for the same (run_id, op_id, stage_id, producer,
    // result) tuple dedups via content addressing — the attestation
    // log's `put` is a no-op on existing attestation_ids.
    let (s, _tmp) = fresh();
    let (op, t) = make_add_op("fac", "stg-1");
    let op_id = s.apply_operation(DEFAULT_BRANCH, op, t).unwrap();

    s.record_op_trace("run-abc", "fac", &op_id,
        lex_vcs::AttestationResult::Passed, producer()).unwrap();
    s.record_op_trace("run-abc", "fac", &op_id,
        lex_vcs::AttestationResult::Passed, producer()).unwrap();

    let attlog = s.attestation_log().unwrap();
    let by_run = attlog.list_for_run(&"run-abc".to_string()).unwrap();
    assert_eq!(by_run.len(), 1, "duplicate emit must dedup by attestation_id");
}

#[test]
fn record_op_trace_unknown_op_errors() {
    let (s, _tmp) = fresh();
    let bogus = lex_vcs::OpId::from("does-not-exist".to_string());
    let err = s.record_op_trace("run-abc", "fac", &bogus,
        lex_vcs::AttestationResult::Passed, producer());
    assert!(matches!(err, Err(lex_store::StoreError::UnknownOp(_))));
}

#[test]
fn record_run_committed_ops_since_walks_diff() {
    // Snapshot pre-run head, commit several ops, snapshot post-run
    // head, call the snapshot-diff helper. Every committed op gets
    // a Trace attestation linking it to the run.
    let (s, _tmp) = fresh();
    let pre_head = s.get_branch(DEFAULT_BRANCH).unwrap()
        .and_then(|b| b.head_op);  // None on a fresh store

    let (op_a, t_a) = make_add_op("fa", "stg-fa");
    let id_a = s.apply_operation(DEFAULT_BRANCH, op_a, t_a).unwrap();
    let op_b = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fb".into(), stage_id: "stg-fb".into(),
            effects: BTreeSet::new(), budget_cost: None,
        },
        [id_a.clone()],
    );
    let id_b = s.apply_operation(DEFAULT_BRANCH, op_b, StageTransition::Create {
        sig_id: "fb".into(), stage_id: "stg-fb".into(),
    }).unwrap();

    let n = s.record_run_committed_ops_since(
        "run-xyz",
        "main",
        DEFAULT_BRANCH,
        pre_head.as_ref(),
        lex_vcs::AttestationResult::Passed,
        producer(),
    ).unwrap();
    assert_eq!(n, 2, "both ops should be linked to the run");

    let attlog = s.attestation_log().unwrap();
    let by_run = attlog.list_for_run(&"run-xyz".to_string()).unwrap();
    assert_eq!(by_run.len(), 2);
    let op_ids: BTreeSet<_> = by_run.iter()
        .filter_map(|a| a.op_id.clone())
        .collect();
    assert!(op_ids.contains(&id_a));
    assert!(op_ids.contains(&id_b));
}

#[test]
fn record_run_committed_ops_since_empty_when_no_ops_committed() {
    // The most common case: a traced run that doesn't commit ops.
    let (s, _tmp) = fresh();
    // Seed one op so head_op exists.
    let (op, t) = make_add_op("seed", "stg-seed");
    let seed_id = s.apply_operation(DEFAULT_BRANCH, op, t).unwrap();

    // Snapshot AFTER the seed — pretend the run starts here and
    // commits nothing.
    let pre_run_head = Some(seed_id.clone());
    let n = s.record_run_committed_ops_since(
        "run-quiet",
        "main",
        DEFAULT_BRANCH,
        pre_run_head.as_ref(),
        lex_vcs::AttestationResult::Passed,
        producer(),
    ).unwrap();
    assert_eq!(n, 0, "no new ops since pre_run_head");
    let attlog = s.attestation_log().unwrap();
    let by_run = attlog.list_for_run(&"run-quiet".to_string()).unwrap();
    assert!(by_run.is_empty());
}

#[test]
fn record_run_committed_ops_since_unknown_branch_returns_zero() {
    // The branch doesn't exist (no head_op) — emit nothing rather
    // than erroring. `lex run` may be invoked in a fresh store
    // before any branch has been advanced.
    let (s, _tmp) = fresh();
    let n = s.record_run_committed_ops_since(
        "run-empty", "main", "ghost", None,
        lex_vcs::AttestationResult::Passed, producer(),
    ).unwrap();
    assert_eq!(n, 0);
}
