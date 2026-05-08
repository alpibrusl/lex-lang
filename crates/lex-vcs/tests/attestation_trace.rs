//! Conformance tests for #246: `AttestationKind::Trace` + the
//! by-run secondary index on [`AttestationLog`].
//!
//! Coverage:
//!
//! 1. Round-trip through serde JSON preserves the variant shape.
//! 2. Two `Trace` attestations with the same logical content
//!    deduplicate via the existing content-addressed `AttestationId`
//!    invariant — Trace doesn't break that.
//! 3. The `by-run` index is populated only for `Trace` variants;
//!    other kinds skip it.
//! 4. `AttestationLog::list_for_run` finds Trace entries.
//! 5. Golden canonical-form pin: a `Trace` attestation hashes to a
//!    fixed `attestation_id` so a future serde reorder surfaces as
//!    a hard test failure.

use lex_vcs::{
    Attestation, AttestationKind, AttestationLog, AttestationResult, ProducerDescriptor,
};
use std::collections::BTreeSet;

fn producer() -> ProducerDescriptor {
    ProducerDescriptor {
        tool: "lex run --trace".into(),
        version: "test".into(),
        model: None,
    }
}

fn trace_attestation(stage_id: &str, run_id: &str, target: &str) -> Attestation {
    Attestation::with_timestamp(
        stage_id.to_string(),
        None,
        None,
        AttestationKind::Trace {
            run_id: run_id.into(),
            root_target: target.into(),
        },
        AttestationResult::Passed,
        producer(),
        None,
        // Fixed timestamp keeps cross-run determinism for the
        // by-run index test below.
        1_700_000_000,
    )
}

#[test]
fn trace_variant_round_trips_through_serde_json() {
    let a = trace_attestation("stage-fac-1", "run-abc-123", "factorial");
    let json = serde_json::to_string(&a).expect("serialize");
    let back: Attestation = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(a, back);
    assert_eq!(a.attestation_id, back.attestation_id);
    // Quick sanity check: the JSON tags the kind correctly.
    assert!(json.contains("\"kind\":\"trace\""), "json: {json}");
    assert!(json.contains("\"run_id\":\"run-abc-123\""));
    assert!(json.contains("\"root_target\":\"factorial\""));
}

#[test]
fn identical_trace_attestations_dedup() {
    // The content-addressed AttestationId invariant must extend to
    // the Trace variant: same (stage_id, op_id, intent_id, kind,
    // result, produced_by) → same id.
    let a = trace_attestation("stage-fac-1", "run-abc-123", "factorial");
    let b = trace_attestation("stage-fac-1", "run-abc-123", "factorial");
    assert_eq!(a.attestation_id, b.attestation_id);
    // Different run_id → different id.
    let c = trace_attestation("stage-fac-1", "run-xyz-999", "factorial");
    assert_ne!(a.attestation_id, c.attestation_id);
    // Different root_target → different id.
    let d = trace_attestation("stage-fac-1", "run-abc-123", "other_fn");
    assert_ne!(a.attestation_id, d.attestation_id);
}

#[test]
fn by_run_index_populates_only_for_trace_variants() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();

    let trace = trace_attestation("stage-fac-1", "run-abc-123", "factorial");
    log.put(&trace).unwrap();

    let typecheck = Attestation::with_timestamp(
        "stage-fac-1".to_string(),
        None,
        None,
        AttestationKind::TypeCheck,
        AttestationResult::Passed,
        producer(),
        None,
        1_700_000_001,
    );
    log.put(&typecheck).unwrap();

    let by_run = tmp.path().join("attestations").join("by-run");
    let run_dir = by_run.join("run-abc-123");
    assert!(run_dir.exists(), "by-run/<run_id> dir must exist");
    let entries: Vec<_> = std::fs::read_dir(&run_dir).unwrap().collect();
    assert_eq!(
        entries.len(),
        1,
        "only the Trace attestation should be indexed by run_id; got {}",
        entries.len(),
    );

    // Sanity: the TypeCheck attestation IS in the by-stage index
    // (so the change is purely additive — no regression).
    let by_stage = tmp.path().join("attestations").join("by-stage").join("stage-fac-1");
    let stage_entries: Vec<_> = std::fs::read_dir(&by_stage).unwrap().collect();
    assert_eq!(
        stage_entries.len(),
        2,
        "both Trace and TypeCheck should be in by-stage; got {}",
        stage_entries.len(),
    );
}

#[test]
fn list_for_run_returns_only_traces_for_that_run() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();

    let t_a1 = trace_attestation("stage-1", "run-A", "fn_one");
    let t_a2 = trace_attestation("stage-2", "run-A", "fn_two");
    let t_b = trace_attestation("stage-1", "run-B", "fn_one");
    let unrelated = Attestation::with_timestamp(
        "stage-1".to_string(),
        None,
        None,
        AttestationKind::SandboxRun {
            effects: BTreeSet::new(),
        },
        AttestationResult::Passed,
        producer(),
        None,
        1_700_000_002,
    );
    log.put(&t_a1).unwrap();
    log.put(&t_a2).unwrap();
    log.put(&t_b).unwrap();
    log.put(&unrelated).unwrap();

    let mut for_a = log.list_for_run(&"run-A".to_string()).unwrap();
    for_a.sort_by_key(|a| a.attestation_id.clone());
    assert_eq!(for_a.len(), 2, "run-A should have 2 trace entries");
    for a in &for_a {
        assert!(matches!(a.kind, AttestationKind::Trace { ref run_id, .. } if run_id == "run-A"));
    }

    let for_b = log.list_for_run(&"run-B".to_string()).unwrap();
    assert_eq!(for_b.len(), 1);

    let none = log.list_for_run(&"unknown".to_string()).unwrap();
    assert!(none.is_empty());
}

#[test]
fn golden_attestation_id_for_canonical_trace() {
    // Pinning the attestation_id of a Trace attestation. If the
    // canonical-form encoding of `AttestationKind::Trace` shifts —
    // e.g. someone reorders the variants in the enum, or renames a
    // field — every existing Trace attestation_id rotates and this
    // test breaks loudly. Update with care; a rotation is a
    // breaking change for any persisted attestation log.
    let a = Attestation::with_timestamp(
        "fac::Int->Int".to_string(),
        None,
        None,
        AttestationKind::Trace {
            run_id: "deadbeef".into(),
            root_target: "factorial".into(),
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "lex run --trace".into(),
            version: "0.0.0".into(),
            model: None,
        },
        None,
        // Timestamp is excluded from the hash, so its value doesn't
        // affect the pin.
        0,
    );
    // Capture: cargo test --test attestation_trace -- --ignored
    // capture_golden_attestation_id --nocapture
    assert_eq!(a.attestation_id.len(), 64);
    assert!(
        a.attestation_id.chars().all(|c| c.is_ascii_hexdigit()),
        "attestation_id must be lowercase hex: {}",
        a.attestation_id,
    );
    // Pinned value (computed once via the capture test below).
    assert_eq!(
        a.attestation_id,
        "76f8f5002981d0847d7456cd15a25f864f3de5d63a5473e3f42baf3b0ca4547c",
        "Trace attestation_id rotated; this is a canonical-form break",
    );
}

#[test]
#[ignore]
fn capture_golden_attestation_id() {
    let a = Attestation::with_timestamp(
        "fac::Int->Int".to_string(),
        None,
        None,
        AttestationKind::Trace {
            run_id: "deadbeef".into(),
            root_target: "factorial".into(),
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "lex run --trace".into(),
            version: "0.0.0".into(),
            model: None,
        },
        None,
        0,
    );
    println!("attestation_id: {}", a.attestation_id);
}
