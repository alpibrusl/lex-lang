//! Conformance test for #256: the walk-back producer-block gate.
//!
//! The scenario from #256's issue body:
//!
//! 1. Tool X publishes op A1 (attests stage S1 with a Spec
//!    attestation at t=100). Branch advances to A1.
//! 2. Time passes. Branch advances to A2, A3, etc.
//! 3. Admin retro-blocks tool X at t=2000.
//! 4. Some agent now publishes op A_new. The naive #248 gate
//!    checks A_new's stage attestations (just the auto-emitted
//!    TypeCheck from `lex-store`); lex-store isn't blocked, so
//!    the gate passes — even though A1 in the chain is
//!    contaminated.
//!
//! With #256, the gate walks back from `head_op` toward
//! `last_gate_checkpoint` and runs `check_producer_block` on
//! every ancestor's attestable stages. The retro-block
//! invalidates the checkpoint, so the next advance re-walks the
//! whole chain and refuses.

use lex_ast::canonicalize_program;
use lex_store::{Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_vcs::{
    Attestation, AttestationKind, AttestationResult, ProducerDescriptor,
};
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

fn pure_op(sig: &str, stg: &str) -> (Operation, StageTransition) {
    (
        Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stg.into(),
                effects: BTreeSet::new(),
                budget_cost: None,
            },
            [],
        ),
        StageTransition::Create {
            sig_id: sig.into(),
            stage_id: stg.into(),
        },
    )
}

fn modify_op(sig: &str, parent: &lex_vcs::OpId, from: &str, to: &str)
    -> (Operation, StageTransition)
{
    (
        Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
                from_budget: None,
                to_budget: None,
            },
            [parent.clone()],
        ),
        StageTransition::Replace {
            sig_id: sig.into(),
            from: from.into(),
            to: to.into(),
        },
    )
}

fn pure_candidate(name: &str) -> Vec<lex_ast::Stage> {
    parse(&format!("fn {name}(n :: Int) -> Int {{ n }}\n"))
}

#[test]
fn walk_back_gate_refuses_advance_when_ancestor_is_contaminated() {
    let (s, _tmp) = fresh();

    // 1. Publish op A1 with stage stg-1. Branch advances clean.
    let (op_a, t_a) = pure_op("fac", "stg-1");
    s.apply_operation_checked(DEFAULT_BRANCH, op_a, t_a, &pure_candidate("fac"))
        .expect("clean publish of A1");

    // Manually emit a Spec attestation on stg-1 produced by tool-X.
    // This represents an external producer (#186 spec checker)
    // that ran after A1 landed.
    let log = s.attestation_log().unwrap();
    let head_after_a = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.clone();
    let spec_att = Attestation::with_timestamp(
        "stg-1".to_string(),
        head_after_a.clone(),
        None,
        AttestationKind::Spec {
            spec_id: "fac-spec".into(),
            method: lex_vcs::SpecMethod::Random,
            trials: Some(100),
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "tool-X".into(),
            version: "1.0".into(),
            model: None,
        },
        None,
        100,
    );
    log.put(&spec_att).unwrap();

    // 2. Advance the branch a couple more times. Each advance
    //    runs the gate; since tool-X isn't blocked yet, all pass.
    let head_a = head_after_a.clone().unwrap();
    let (op_b, t_b) = modify_op("fac", &head_a, "stg-1", "stg-2");
    s.apply_operation_checked(DEFAULT_BRANCH, op_b, t_b, &pure_candidate("fac"))
        .expect("clean publish of B");

    // 3. Retro-block tool-X. This invalidates last_gate_checkpoint
    //    on every branch.
    let block_att = Attestation::with_timestamp(
        "tool-X".to_string(),
        None,
        None,
        AttestationKind::ProducerBlock {
            tool_id: "tool-X".into(),
            reason: "compromised".into(),
            blocked_at: 50, // before t=100, so the spec att at t=100 IS contaminated
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "lex attest retro-block".into(),
            version: "0".into(),
            model: None,
        },
        None,
        2000,
    );
    log.put(&block_att).unwrap();
    let invalidated = s.invalidate_gate_checkpoints().unwrap();
    assert_eq!(invalidated, 1, "main's checkpoint should have been cleared");

    // 4. Try to advance again. With #256, the gate now walks back
    //    over A1 and discovers stg-1 has a contaminated Spec
    //    attestation. The advance refuses.
    let head_b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.clone().unwrap();
    let (op_c, t_c) = modify_op("fac", &head_b, "stg-2", "stg-3");
    let err = s.apply_operation_checked(DEFAULT_BRANCH, op_c, t_c, &pure_candidate("fac"))
        .expect_err("post-retro-block advance must refuse");
    let blocked = match err {
        StoreError::ProducerBlocked(b) => b,
        other => panic!("expected ProducerBlocked, got {other:?}"),
    };
    assert_eq!(blocked.tool_id, "tool-X");
    assert_eq!(blocked.stage_id, "stg-1",
        "the contamination is on stg-1 (an ancestor), not the new op's stage");
}

#[test]
fn walk_back_skipped_when_checkpoint_equals_head() {
    // Steady-state: every successful advance moves the checkpoint
    // to the new head, so the next advance has nothing to walk back
    // over (only the new op's candidate is checked). This test
    // verifies the optimization works — that we don't redundantly
    // re-walk on every advance.
    let (s, _tmp) = fresh();
    let (op_a, t_a) = pure_op("fac", "stg-1");
    s.apply_operation_checked(DEFAULT_BRANCH, op_a, t_a, &pure_candidate("fac"))
        .expect("clean publish");

    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(
        b.head_op, b.last_gate_checkpoint,
        "after successful advance, checkpoint must equal head",
    );
}

#[test]
fn unblock_clears_contamination_and_allows_advance() {
    // Scenario: contamination exists, retro-block fires, advance
    // refuses. Then admin retro-unblocks tool-X. The next advance
    // re-walks and (because the most-recent verdict is unblock)
    // accepts the chain.
    let (s, _tmp) = fresh();

    let (op_a, t_a) = pure_op("fac", "stg-1");
    s.apply_operation_checked(DEFAULT_BRANCH, op_a, t_a, &pure_candidate("fac"))
        .expect("clean publish of A1");

    let log = s.attestation_log().unwrap();
    let head_a = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.clone().unwrap();
    let spec_att = Attestation::with_timestamp(
        "stg-1".to_string(),
        Some(head_a.clone()),
        None,
        AttestationKind::Spec {
            spec_id: "fac-spec".into(),
            method: lex_vcs::SpecMethod::Random,
            trials: Some(100),
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "tool-X".into(),
            version: "1.0".into(),
            model: None,
        },
        None,
        100,
    );
    log.put(&spec_att).unwrap();

    // Block at t=50 (before the spec att at t=100).
    let block_att = Attestation::with_timestamp(
        "tool-X".to_string(),
        None,
        None,
        AttestationKind::ProducerBlock {
            tool_id: "tool-X".into(),
            reason: "compromised".into(),
            blocked_at: 50,
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "lex attest retro-block".into(),
            version: "0".into(),
            model: None,
        },
        None,
        2000,
    );
    log.put(&block_att).unwrap();
    s.invalidate_gate_checkpoints().unwrap();

    // Verify advance refuses.
    let (op_b, t_b) = modify_op("fac", &head_a, "stg-1", "stg-2");
    let err = s.apply_operation_checked(DEFAULT_BRANCH, op_b, t_b, &pure_candidate("fac"));
    assert!(matches!(err, Err(StoreError::ProducerBlocked(_))),
        "expected ProducerBlocked, got {err:?}");

    // Now unblock at t=3000 (latest verdict).
    let unblock_att = Attestation::with_timestamp(
        "tool-X".to_string(),
        None,
        None,
        AttestationKind::ProducerUnblock {
            tool_id: "tool-X".into(),
            reason: "vendor patched".into(),
            unblocked_at: 3000,
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "lex attest retro-block".into(),
            version: "0".into(),
            model: None,
        },
        None,
        3000,
    );
    log.put(&unblock_att).unwrap();
    s.invalidate_gate_checkpoints().unwrap();

    // Same advance attempt now succeeds — the active block map is
    // empty after the unblock.
    let (op_b2, t_b2) = modify_op("fac", &head_a, "stg-1", "stg-2");
    s.apply_operation_checked(DEFAULT_BRANCH, op_b2, t_b2, &pure_candidate("fac"))
        .expect("post-unblock advance should succeed");
}

#[test]
fn pre_256_branch_files_without_checkpoint_walk_from_genesis() {
    // Backward-compat: a branch.json from before #256 has no
    // last_gate_checkpoint field. Default deserializes to None,
    // which forces a one-time walk from genesis. This test seeds
    // a chain, manually clears the checkpoint to simulate an
    // upgraded store, and verifies the next advance still works
    // (no contamination → walk passes → advance succeeds).
    let (s, _tmp) = fresh();
    let (op_a, t_a) = pure_op("fac", "stg-1");
    s.apply_operation_checked(DEFAULT_BRANCH, op_a, t_a, &pure_candidate("fac"))
        .expect("clean publish of A");

    // Simulate an upgraded store: rewrite branch.json with
    // last_gate_checkpoint missing.
    let path = s.root().join("branches/main.json");
    let bytes = std::fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value.as_object_mut().unwrap().remove("last_gate_checkpoint");
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    // Reload + verify the field is None.
    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert!(b.last_gate_checkpoint.is_none(), "checkpoint should default to None");

    // Advance. With no contamination in the log, the walk-back
    // from head to genesis passes; the advance succeeds.
    let head = b.head_op.clone().unwrap();
    let (op_b, t_b) = modify_op("fac", &head, "stg-1", "stg-2");
    s.apply_operation_checked(DEFAULT_BRANCH, op_b, t_b, &pure_candidate("fac"))
        .expect("upgraded-store advance should succeed");

    // After the advance, checkpoint should now equal the new head.
    let b2 = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert!(b2.last_gate_checkpoint.is_some());
    assert_eq!(b2.last_gate_checkpoint, b2.head_op);
}
