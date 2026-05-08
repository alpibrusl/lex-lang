//! Conformance test for #258: attestation-cascade migration.
//!
//! When `OperationFormat` evolves and `OpId`s rotate (#244),
//! every attestation whose `op_id` references a rotated op is
//! left dangling — its stored `op_id` field points at a deleted
//! record, and its own `attestation_id` (content-addressed
//! including `op_id`) is now stale.
//!
//! `lex_vcs::migrate::plan_attestation_migration` +
//! `apply_attestation_migration` close that gap. After running,
//! every dangling attestation is replaced by a new one with the
//! new `op_id` and a freshly-derived `attestation_id`.

use lex_vcs::migrate::{
    apply_attestation_migration, apply_migration, plan_attestation_migration,
    plan_migration_with_encoder,
};
use lex_vcs::{
    Attestation, AttestationKind, AttestationLog, AttestationResult, Operation, OperationFormat,
    OperationKind, OperationRecord, OpLog, ProducerDescriptor, StageTransition,
};
use std::collections::BTreeSet;

fn add_op(parent: Option<&str>, sig: &str, stg: &str) -> OperationRecord {
    let parents: Vec<String> = parent.map(|p| p.to_string()).into_iter().collect();
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

fn typecheck(stage_id: &str, op_id: &str) -> Attestation {
    Attestation::with_timestamp(
        stage_id.to_string(),
        Some(op_id.into()),
        None,
        AttestationKind::TypeCheck,
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "test".into(),
            version: "0".into(),
            model: None,
        },
        None,
        1_700_000_000,
    )
}

fn fake_v2_encoder(op: &Operation) -> Vec<u8> {
    let mut bytes = op.canonical_bytes_in(OperationFormat::V1);
    bytes.extend_from_slice(b";v2-stub");
    bytes
}

#[test]
fn cascade_migrates_typecheck_attestations_to_new_op_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();
    let attest_log = AttestationLog::open(tmp.path()).unwrap();

    // Seed: two ops, each with a TypeCheck attestation.
    let a = add_op(None, "fac", "stg-1");
    log.put(&a).unwrap();
    let b = add_op(None, "double", "stg-2");
    log.put(&b).unwrap();
    let att_a = typecheck("stg-1", &a.op_id);
    let att_b = typecheck("stg-2", &b.op_id);
    attest_log.put(&att_a).unwrap();
    attest_log.put(&att_b).unwrap();

    // Migrate the op log under the fake-V2 encoder. Every op_id
    // rotates.
    let plan = plan_migration_with_encoder(&log, OperationFormat::V1, fake_v2_encoder).unwrap();
    apply_migration(&log, &plan).unwrap();
    let mapping = plan.mapping();

    // The pre-cascade attestation log: both attestations dangle —
    // their `op_id` references rotated ops that no longer exist.
    let pre_cascade = attest_log.list_all().unwrap();
    assert_eq!(pre_cascade.len(), 2);
    for a in &pre_cascade {
        let op_id = a.op_id.as_ref().unwrap();
        assert!(log.get(op_id).unwrap().is_none(),
            "pre-cascade attestation {} still references unrotated op_id {op_id}",
            a.attestation_id);
    }

    // Run the cascade.
    let steps = plan_attestation_migration(&attest_log, &mapping).unwrap();
    assert_eq!(steps.len(), 2, "both attestations should be in the plan");
    apply_attestation_migration(&attest_log, &steps).unwrap();

    // Post-cascade: every attestation references a *new* op_id
    // that exists in the rotated log. The old attestation_ids
    // are gone.
    let post = attest_log.list_all().unwrap();
    assert_eq!(post.len(), 2, "two new attestations replace the two old ones");
    for a in &post {
        let op_id = a.op_id.as_ref().unwrap();
        assert!(log.get(op_id).unwrap().is_some(),
            "post-cascade attestation {} should reference a live op_id",
            a.attestation_id);
        assert!(mapping.values().any(|new| new == op_id),
            "post-cascade op_id {op_id} should be a new mapping target");
    }

    // Old attestation files are deleted.
    for old in [&att_a, &att_b] {
        assert!(attest_log.get(&old.attestation_id).unwrap().is_none(),
            "old attestation {} should be removed", old.attestation_id);
    }
}

#[test]
fn cascade_skips_attestations_with_no_op_id() {
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();
    let attest_log = AttestationLog::open(tmp.path()).unwrap();

    let a = add_op(None, "fac", "stg-1");
    log.put(&a).unwrap();
    // An Override attestation with op_id: None — doesn't
    // participate in the cascade.
    let override_att = Attestation::with_timestamp(
        "stg-1".to_string(),
        None,
        None,
        AttestationKind::Override {
            actor: "alice".into(),
            reason: "x".into(),
            target_attestation_id: None,
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "test".into(),
            version: "0".into(),
            model: None,
        },
        None,
        1,
    );
    let typecheck_att = typecheck("stg-1", &a.op_id);
    attest_log.put(&override_att).unwrap();
    attest_log.put(&typecheck_att).unwrap();

    let plan = plan_migration_with_encoder(&log, OperationFormat::V1, fake_v2_encoder).unwrap();
    apply_migration(&log, &plan).unwrap();
    let mapping = plan.mapping();

    let steps = plan_attestation_migration(&attest_log, &mapping).unwrap();
    assert_eq!(steps.len(), 1, "only the TypeCheck (with op_id) is in the plan");
    assert_eq!(steps[0].old.attestation_id, typecheck_att.attestation_id);
    apply_attestation_migration(&attest_log, &steps).unwrap();

    // The Override stays intact (same attestation_id).
    let post = attest_log.list_all().unwrap();
    let override_post = post.iter().find(|a| matches!(a.kind, AttestationKind::Override { .. }))
        .expect("Override should still exist");
    assert_eq!(override_post.attestation_id, override_att.attestation_id);
}

#[test]
fn cascade_is_idempotent_on_re_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();
    let attest_log = AttestationLog::open(tmp.path()).unwrap();

    let a = add_op(None, "fac", "stg-1");
    log.put(&a).unwrap();
    let att = typecheck("stg-1", &a.op_id);
    attest_log.put(&att).unwrap();

    let plan = plan_migration_with_encoder(&log, OperationFormat::V1, fake_v2_encoder).unwrap();
    apply_migration(&log, &plan).unwrap();
    let mapping = plan.mapping();

    let steps = plan_attestation_migration(&attest_log, &mapping).unwrap();
    apply_attestation_migration(&attest_log, &steps).unwrap();

    // Re-running plan_attestation_migration on the now-cascaded
    // log: the attestations reference *new* op_ids, none of which
    // are in the original mapping (mapping keys are the old
    // op_ids). So the second plan is empty.
    let steps2 = plan_attestation_migration(&attest_log, &mapping).unwrap();
    assert!(steps2.is_empty(),
        "second cascade plan should be empty; cascade is one-shot");
}

#[test]
fn cascade_preserves_by_stage_index() {
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();
    let attest_log = AttestationLog::open(tmp.path()).unwrap();

    let a = add_op(None, "fac", "stg-1");
    log.put(&a).unwrap();
    let att = typecheck("stg-1", &a.op_id);
    attest_log.put(&att).unwrap();

    let plan = plan_migration_with_encoder(&log, OperationFormat::V1, fake_v2_encoder).unwrap();
    apply_migration(&log, &plan).unwrap();
    let mapping = plan.mapping();
    let steps = plan_attestation_migration(&attest_log, &mapping).unwrap();
    apply_attestation_migration(&attest_log, &steps).unwrap();

    // by-stage index lookup must return the new attestation, not
    // the old one.
    let for_stage = attest_log.list_for_stage(&"stg-1".to_string()).unwrap();
    assert_eq!(for_stage.len(), 1);
    let post = &for_stage[0];
    assert_ne!(post.attestation_id, att.attestation_id,
        "by-stage should return the *new* attestation");
    let new_op_id = mapping.get(&a.op_id).expect("a was rotated");
    assert_eq!(post.op_id.as_deref(), Some(new_op_id.as_str()));
}

#[test]
fn empty_mapping_is_a_no_op() {
    let tmp = tempfile::tempdir().unwrap();
    let attest_log = AttestationLog::open(tmp.path()).unwrap();
    let empty_mapping = std::collections::BTreeMap::new();
    let steps = plan_attestation_migration(&attest_log, &empty_mapping).unwrap();
    assert!(steps.is_empty());
}
