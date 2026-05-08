//! Conformance tests for #248: retroactive producer quarantine via
//! `AttestationKind::ProducerBlock` + the `check_producer_block`
//! gate.
//!
//! Coverage:
//!
//! 1. Default-permissive: no `ProducerBlock` attestations → gate
//!    passes.
//! 2. ProducerBlock attestation → gate refuses to advance over an
//!    op whose stage carries an attestation produced by the
//!    quarantined tool *at or after* `blocked_at`. Pre-block
//!    attestations are grandfathered.
//! 3. `ProducerUnblock` reverses the block by timestamp; the gate
//!    honors the latest verdict.
//! 4. The structured `ProducerBlocked` envelope shape matches the
//!    issue's HTTP API expectation.
//! 5. Self-references (a tool's own ProducerBlock attestations,
//!    stored at `stage_id == tool_id`) don't trip the gate.

use lex_store::policy::{check_producer_block, ProducerBlocked};
use lex_vcs::{
    Attestation, AttestationKind, AttestationLog, AttestationResult, ProducerDescriptor,
};
use std::collections::BTreeSet;

fn producer(tool: &str) -> ProducerDescriptor {
    ProducerDescriptor {
        tool: tool.into(),
        version: "test".into(),
        model: None,
    }
}

fn typecheck_attestation(stage_id: &str, op_id: &str, by_tool: &str, at: u64) -> Attestation {
    Attestation::with_timestamp(
        stage_id.to_string(),
        Some(op_id.into()),
        None,
        AttestationKind::TypeCheck,
        AttestationResult::Passed,
        producer(by_tool),
        None,
        at,
    )
}

fn producer_block_attestation(tool_id: &str, reason: &str, at: u64) -> Attestation {
    Attestation::with_timestamp(
        // Stored under stage_id == tool_id so the by-stage index
        // doubles as a by-tool lookup.
        tool_id.to_string(),
        None,
        None,
        AttestationKind::ProducerBlock {
            tool_id: tool_id.into(),
            reason: reason.into(),
            blocked_at: at,
        },
        AttestationResult::Passed,
        producer("lex attest retro-block"),
        None,
        at,
    )
}

fn producer_unblock_attestation(tool_id: &str, reason: &str, at: u64) -> Attestation {
    Attestation::with_timestamp(
        tool_id.to_string(),
        None,
        None,
        AttestationKind::ProducerUnblock {
            tool_id: tool_id.into(),
            reason: reason.into(),
            unblocked_at: at,
        },
        AttestationResult::Passed,
        producer("lex attest retro-block"),
        None,
        at,
    )
}

fn candidate(op_id: &str, stage_id: &str) -> Vec<(String, Option<String>, BTreeSet<String>)> {
    vec![(
        op_id.to_string(),
        Some(stage_id.to_string()),
        BTreeSet::new(),
    )]
}

#[test]
fn no_producer_blocks_means_gate_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    log.put(&typecheck_attestation("stage-1", "op-1", "lex-store", 100))
        .unwrap();
    let res = check_producer_block(&log, &candidate("op-1", "stage-1"));
    assert!(res.is_ok());
}

#[test]
fn block_after_attestation_does_not_fence_grandfathered_evidence() {
    // Tool X produced an attestation at t=100. Admin retro-blocks
    // X at t=200. The block's `blocked_at` is 200, so the t=100
    // attestation is *before* the cutoff and is not contaminated.
    // The gate passes — retro-blocks are forward-going from the
    // cutoff timestamp, not retroactive to the start of time.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    log.put(&typecheck_attestation("stage-1", "op-1", "tool-X", 100))
        .unwrap();
    log.put(&producer_block_attestation(
        "tool-X",
        "compromised at 200",
        200,
    ))
    .unwrap();
    let res = check_producer_block(&log, &candidate("op-1", "stage-1"));
    assert!(
        res.is_ok(),
        "attestation at 100 with block at 200 must not fire: {res:?}",
    );
}

#[test]
fn block_with_attestation_at_or_after_cutoff_refuses_advance() {
    // Tool X produces an attestation at t=200; admin retro-blocks
    // X at t=200 (the simultaneous case). The gate refuses because
    // `attestation.timestamp >= blocked_at`.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let bad = typecheck_attestation("stage-1", "op-1", "tool-X", 250);
    log.put(&bad).unwrap();
    log.put(&producer_block_attestation(
        "tool-X",
        "key leaked at 200",
        200,
    ))
    .unwrap();

    let err = check_producer_block(&log, &candidate("op-1", "stage-1"))
        .expect_err("attestation at 250 with block at 200 must refuse");
    let blocked: ProducerBlocked = err;
    assert_eq!(blocked.op_id, "op-1");
    assert_eq!(blocked.stage_id, "stage-1");
    assert_eq!(blocked.tool_id, "tool-X");
    assert_eq!(blocked.blocked_at, 200);
    assert_eq!(blocked.attestation_at, 250);
    assert_eq!(blocked.attestation_id, bad.attestation_id);
}

#[test]
fn unblock_after_block_clears_the_quarantine() {
    // Block at t=200, unblock at t=300, attestation at t=400.
    // Latest verdict is "unblocked", so the gate passes.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    log.put(&typecheck_attestation("stage-1", "op-1", "tool-X", 400))
        .unwrap();
    log.put(&producer_block_attestation("tool-X", "compromised", 200))
        .unwrap();
    log.put(&producer_unblock_attestation(
        "tool-X",
        "vendor patched and rotated keys",
        300,
    ))
    .unwrap();

    let res = check_producer_block(&log, &candidate("op-1", "stage-1"));
    assert!(res.is_ok(), "unblock should clear the quarantine: {res:?}");
}

#[test]
fn block_after_unblock_re_quarantines() {
    // unblock at t=200, then a fresh block at t=400. attestation
    // at t=500. Latest verdict is "blocked", so the gate refuses.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    log.put(&typecheck_attestation("stage-1", "op-1", "tool-X", 500))
        .unwrap();
    log.put(&producer_unblock_attestation("tool-X", "ok at 200", 200))
        .unwrap();
    log.put(&producer_block_attestation("tool-X", "re-flagged", 400))
        .unwrap();

    let err = check_producer_block(&log, &candidate("op-1", "stage-1"))
        .expect_err("re-block must fire");
    assert_eq!(err.tool_id, "tool-X");
    assert_eq!(err.blocked_at, 400);
}

#[test]
fn envelope_shape_matches_issue_spec() {
    let blocked = ProducerBlocked {
        op_id: "op-deadbeef".into(),
        stage_id: "stage-1".into(),
        tool_id: "claude-code-mcp@v0.3.1".into(),
        blocked_at: 1_700_000_200,
        attestation_at: 1_700_000_400,
        attestation_id: "attestation-feedface".into(),
    };
    let env = blocked.to_envelope();
    assert_eq!(env["error"], "ProducerBlocked");
    assert_eq!(env["op_id"], "op-deadbeef");
    assert_eq!(env["stage_id"], "stage-1");
    assert_eq!(env["tool_id"], "claude-code-mcp@v0.3.1");
    assert_eq!(env["blocked_at"], 1_700_000_200);
    assert_eq!(env["attestation_at"], 1_700_000_400);
    assert_eq!(env["attestation_id"], "attestation-feedface");
}

#[test]
fn producer_block_self_references_dont_trip_the_gate() {
    // ProducerBlock attestations are stored at `stage_id ==
    // tool_id`. If the candidate stage_id happens to match a
    // tool_id (rare but possible in tests), the gate must skip
    // ProducerBlock / ProducerUnblock attestations themselves —
    // otherwise blocking a tool would also block any op whose
    // stage_id collides with the tool name.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    // ProducerBlock for "tool-X" — produced by "lex attest retro-block".
    log.put(&producer_block_attestation("tool-X", "x", 200))
        .unwrap();
    // Now also retro-block the *retro-block tool itself*: a
    // pathological self-reference. The gate must not fire on the
    // existing ProducerBlock attestation, even though that
    // attestation's producer (`lex attest retro-block`) is now in
    // the active block set.
    log.put(&producer_block_attestation(
        "lex attest retro-block",
        "ironic",
        300,
    ))
    .unwrap();

    // Candidate is the tool-X stage itself. The only attestations
    // there are ProducerBlock/Unblock records for tool-X — those
    // are skipped by the gate so the advance succeeds.
    let res = check_producer_block(&log, &candidate("op-1", "tool-X"));
    assert!(
        res.is_ok(),
        "self-referenced producer-block records must not contaminate themselves: {res:?}",
    );
}

#[test]
fn pre_245_records_without_producer_block_field_are_unaffected() {
    // Backward-compat: an attestation log from before #248 has no
    // ProducerBlock / ProducerUnblock records. The gate is a no-op
    // — same behavior as a fresh store.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    log.put(&typecheck_attestation("stage-1", "op-1", "lex-store", 100))
        .unwrap();
    let res = check_producer_block(&log, &candidate("op-1", "stage-1"));
    assert!(res.is_ok());
}
