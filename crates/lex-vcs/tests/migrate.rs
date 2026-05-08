//! Conformance test for #244: write a small V1 DAG, simulate a
//! future format with a custom encoder, run the migration, and
//! assert that every `OpId` rotated and parent references survived
//! the rewrite.
//!
//! The "V2 stub" here is **not** a production [`OperationFormat`]
//! variant — it's a test-only encoder closure that adds a synthetic
//! suffix to V1's pre-image bytes. That's enough to force every
//! SHA-256 to rotate without polluting production code with a
//! placeholder variant; the migration mechanism is exercised end-
//! to-end exactly as it would be when a real V2 lands.

use lex_vcs::migrate::{apply_migration, plan_migration_with_encoder};
use lex_vcs::{
    Operation, OperationFormat, OperationKind, OperationRecord, OpLog, StageTransition,
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
            },
            parents,
        ),
        StageTransition::Create {
            sig_id: sig.into(),
            stage_id: stg.into(),
        },
    )
}

fn modify_op(parent: &str, sig: &str, from: &str, to: &str) -> OperationRecord {
    OperationRecord::new(
        Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
            },
            [parent.to_string()],
        ),
        StageTransition::Replace {
            sig_id: sig.into(),
            from: from.into(),
            to: to.into(),
        },
    )
}

/// Test-only "V2" encoder. Adds a constant suffix outside the JSON
/// envelope so every pre-image is byte-different from V1. Equivalent
/// in spirit to the real schema break a future variant would
/// introduce; equivalent in mechanics for the migration tool.
fn fake_v2_encoder(op: &Operation) -> Vec<u8> {
    let mut bytes = op.canonical_bytes_in(OperationFormat::V1);
    bytes.extend_from_slice(b";v2-stub");
    bytes
}

#[test]
fn migration_rotates_every_op_id_and_preserves_dag_integrity() {
    // Build a small DAG:
    //
    //     a
    //     |
    //     b
    //     |
    //     c    (chain)
    //
    // Run the migration with a "V2" encoder that produces different
    // bytes from V1. Assert: every old op_id is gone; every new
    // op_id exists; the new `c` references the new `b` as its
    // parent (DAG integrity preserved through the remap).
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();

    let a = add_op(None, "fac", "s0");
    log.put(&a).unwrap();
    let b = modify_op(&a.op_id, "fac", "s0", "s1");
    log.put(&b).unwrap();
    let c = modify_op(&b.op_id, "fac", "s1", "s2");
    log.put(&c).unwrap();

    let plan =
        plan_migration_with_encoder(&log, OperationFormat::V1, fake_v2_encoder).unwrap();

    // The plan reports one step per op, in topological order.
    assert_eq!(plan.steps.len(), 3);
    let order: Vec<_> = plan.steps.iter().map(|s| s.old_op_id.as_str()).collect();
    let pos = |id: &str| order.iter().position(|x| *x == id).unwrap();
    assert!(pos(&a.op_id) < pos(&b.op_id), "a must precede b in topo order");
    assert!(pos(&b.op_id) < pos(&c.op_id), "b must precede c in topo order");

    // Every op_id rotates (the V2 encoder produces a different pre-
    // image so the SHA-256 differs).
    for step in &plan.steps {
        assert_ne!(
            step.old_op_id, step.new_op_id,
            "op {} did not rotate under the V2 encoder",
            step.old_op_id,
        );
    }

    // Parent references in the new records point to *new* op_ids.
    let new_b = plan
        .steps
        .iter()
        .find(|s| s.old_op_id == b.op_id)
        .expect("b is in plan");
    let new_a = plan
        .steps
        .iter()
        .find(|s| s.old_op_id == a.op_id)
        .expect("a is in plan");
    let new_c = plan
        .steps
        .iter()
        .find(|s| s.old_op_id == c.op_id)
        .expect("c is in plan");
    assert_eq!(new_b.new_record.op.parents, vec![new_a.new_op_id.clone()]);
    assert_eq!(new_c.new_record.op.parents, vec![new_b.new_op_id.clone()]);

    // Apply the migration. All three old files are deleted; the
    // three new files exist; each is readable as an OperationRecord
    // and its op_id matches the planned new id.
    apply_migration(&log, &plan).unwrap();
    assert!(log.get(&a.op_id).unwrap().is_none(), "old a survives");
    assert!(log.get(&b.op_id).unwrap().is_none(), "old b survives");
    assert!(log.get(&c.op_id).unwrap().is_none(), "old c survives");

    let new_a_rec = log.get(&new_a.new_op_id).unwrap().expect("new a present");
    let new_b_rec = log.get(&new_b.new_op_id).unwrap().expect("new b present");
    let new_c_rec = log.get(&new_c.new_op_id).unwrap().expect("new c present");
    assert_eq!(new_a_rec.op_id, new_a.new_op_id);
    assert_eq!(new_b_rec.op.parents, vec![new_a.new_op_id.clone()]);
    assert_eq!(new_c_rec.op.parents, vec![new_b.new_op_id.clone()]);

    // The new records all carry the migration target's format
    // version (here V1, because the encoder is test-only and the
    // production enum doesn't yet have a V2).
    assert_eq!(new_a_rec.format_version, OperationFormat::V1);
    assert_eq!(new_b_rec.format_version, OperationFormat::V1);
    assert_eq!(new_c_rec.format_version, OperationFormat::V1);
}

#[test]
fn migration_handles_a_merge_op_with_two_parents() {
    // DAG:
    //
    //     a
    //    / \
    //   b   c
    //    \ /
    //     m  (Merge with parents [b, c])
    //
    // After migration, `m`'s parents must reference the new b and
    // new c — not the old ones.
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();

    let a = add_op(None, "fac", "s0");
    log.put(&a).unwrap();
    let b = modify_op(&a.op_id, "fac", "s0", "b1");
    log.put(&b).unwrap();
    let c = modify_op(&a.op_id, "fac", "s0", "c1");
    log.put(&c).unwrap();
    let m = OperationRecord::new(
        Operation::new(
            OperationKind::Merge { resolved: 0 },
            [b.op_id.clone(), c.op_id.clone()],
        ),
        StageTransition::Merge {
            entries: Default::default(),
        },
    );
    log.put(&m).unwrap();

    let plan =
        plan_migration_with_encoder(&log, OperationFormat::V1, fake_v2_encoder).unwrap();
    let new_m = plan
        .steps
        .iter()
        .find(|s| s.old_op_id == m.op_id)
        .expect("m in plan");
    let new_b = plan
        .steps
        .iter()
        .find(|s| s.old_op_id == b.op_id)
        .expect("b in plan");
    let new_c = plan
        .steps
        .iter()
        .find(|s| s.old_op_id == c.op_id)
        .expect("c in plan");

    // `Operation::new` sorts parents alphabetically, so the order
    // depends on the new op_ids' relative ordering. Compare as a
    // set.
    let actual: BTreeSet<&String> = new_m.new_record.op.parents.iter().collect();
    let expected: BTreeSet<&String> = [&new_b.new_op_id, &new_c.new_op_id].iter().copied().collect();
    assert_eq!(actual, expected);

    apply_migration(&log, &plan).unwrap();
    assert!(log.get(&m.op_id).unwrap().is_none());
    let new_m_rec = log.get(&new_m.new_op_id).unwrap().expect("new m present");
    let actual: BTreeSet<&String> = new_m_rec.op.parents.iter().collect();
    assert_eq!(actual, expected);
}

#[test]
fn migration_is_idempotent_when_target_matches_source() {
    // Migrating V1 → V1 with the *production* encoder is a no-op:
    // every new op_id equals every old op_id, plan.is_no_op()
    // returns true, and apply leaves the log unchanged.
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();
    let a = add_op(None, "fac", "s0");
    log.put(&a).unwrap();
    let b = modify_op(&a.op_id, "fac", "s0", "s1");
    log.put(&b).unwrap();

    let plan =
        plan_migration_with_encoder(&log, OperationFormat::V1, |op| {
            op.canonical_bytes_in(OperationFormat::V1)
        })
        .unwrap();
    assert!(plan.is_no_op(), "V1→V1 with the V1 encoder should be a no-op");
    apply_migration(&log, &plan).unwrap();
    assert!(log.get(&a.op_id).unwrap().is_some());
    assert!(log.get(&b.op_id).unwrap().is_some());
}

#[test]
fn pre_244_records_without_format_version_field_deserialize_to_v1() {
    // Backward-compat: an OperationRecord written by a pre-#244
    // build doesn't have a `format_version` field at all. Reading
    // it back must yield format_version == V1 (the serde default)
    // so the rest of the system treats the record consistently.
    let json = r#"{
        "op_id": "abc123",
        "op": "add_function",
        "sig_id": "fac",
        "stage_id": "s0",
        "effects": [],
        "produces": { "kind": "create", "sig_id": "fac", "stage_id": "s0" }
    }"#;
    let rec: OperationRecord = serde_json::from_str(json).expect("deserialize legacy record");
    assert_eq!(rec.format_version, OperationFormat::V1);
}

#[test]
fn v1_record_round_trip_does_not_emit_format_version() {
    // Writing back a V1 record must keep the on-disk JSON byte-
    // identical to the pre-#244 shape (no `format_version` field).
    // This is the load-bearing property that adding the field to
    // OperationRecord doesn't itself rotate any OpId or invalidate
    // any existing store.
    let rec = add_op(None, "fac", "s0");
    let json = serde_json::to_string(&rec).expect("serialize");
    assert!(
        !json.contains("format_version"),
        "V1 record emitted format_version: {json}",
    );
}
