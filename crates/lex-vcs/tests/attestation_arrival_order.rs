//! Server-assigned arrival order for the attestation log (lex-hub M3
//! hardening H1). The writer-supplied `timestamp` must never decide which
//! attestation is "latest".

use lex_vcs::{
    Attestation, AttestationKind, AttestationLog, AttestationResult, ProducerDescriptor,
    ReviewVerdict,
};
use std::path::Path;

const STAGE: &str = "stage-under-review";

fn review(reviewer: &str, verdict: ReviewVerdict, ts: u64) -> Attestation {
    let result = match verdict {
        ReviewVerdict::Approve => AttestationResult::Passed,
        ReviewVerdict::Reject => AttestationResult::Failed { detail: "no".into() },
        ReviewVerdict::RequestChanges => AttestationResult::Inconclusive { detail: "hm".into() },
    };
    Attestation::with_timestamp(
        STAGE.to_string(),
        None,
        None,
        AttestationKind::Review { reviewer: reviewer.into(), verdict, notes: None },
        result,
        ProducerDescriptor { tool: format!("t:{reviewer}"), version: "0".into(), model: None },
        None,
        ts,
    )
}

fn ids_in_arrival_order(log: &AttestationLog) -> Vec<String> {
    log.list_for_stage_by_arrival(&STAGE.to_string())
        .unwrap()
        .into_iter()
        .map(|a| a.attestation_id)
        .collect()
}

/// Drop the arrival record, simulating an attestation persisted before the
/// sidecar existed.
fn make_legacy(root: &Path, a: &Attestation) {
    std::fs::remove_file(root.join("attestations").join("arrival").join(&a.attestation_id))
        .unwrap();
}

#[test]
fn a_far_future_timestamp_does_not_outrank_a_later_arrival() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let forged_first = review("mallory", ReviewVerdict::Approve, u64::MAX / 2);
    let genuine_later = review("alice", ReviewVerdict::Reject, 1_700_000_000);
    log.put(&forged_first).unwrap();
    log.put(&genuine_later).unwrap();
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![forged_first.attestation_id.clone(), genuine_later.attestation_id.clone()],
        "arrival order, not the client timestamp, decides which is last"
    );
}

#[test]
fn a_backdated_timestamp_does_not_lose_to_an_earlier_arrival() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let first = review("alice", ReviewVerdict::Approve, 2_000_000_000);
    let second_backdated = review("bob", ReviewVerdict::Reject, 1);
    log.put(&first).unwrap();
    log.put(&second_backdated).unwrap();
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![first.attestation_id.clone(), second_backdated.attestation_id.clone()]
    );
}

#[test]
fn arrival_order_is_stable_across_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let a = review("a", ReviewVerdict::Approve, 900);
    let b = review("b", ReviewVerdict::Reject, 100);
    let c = review("c", ReviewVerdict::RequestChanges, 500);
    {
        let log = AttestationLog::open(tmp.path()).unwrap();
        log.put(&a).unwrap();
        log.put(&b).unwrap();
    }
    // "Restart": a brand new handle over the same directory keeps the
    // counter going rather than restarting from 1.
    let log = AttestationLog::open(tmp.path()).unwrap();
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![a.attestation_id.clone(), b.attestation_id.clone()]
    );
    log.put(&c).unwrap();
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![a.attestation_id.clone(), b.attestation_id.clone(), c.attestation_id.clone()]
    );
    let seqs: Vec<u64> = [&a, &b, &c]
        .iter()
        .map(|x| log.arrival_seq(&x.attestation_id).unwrap().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "strictly increasing: {seqs:?}");
}

#[test]
fn counter_is_rebuilt_from_records_if_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let a = review("a", ReviewVerdict::Approve, 1);
    let b = review("b", ReviewVerdict::Reject, 2);
    log.put(&a).unwrap();
    std::fs::remove_file(tmp.path().join("attestations").join("arrival.seq")).unwrap();
    log.put(&b).unwrap();
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![a.attestation_id.clone(), b.attestation_id.clone()],
        "a lost counter must not restart numbering under existing records"
    );
}

#[test]
fn reputting_the_same_id_does_not_move_its_arrival_position() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let reject = review("alice", ReviewVerdict::Reject, 10);
    let approve = review("bob", ReviewVerdict::Approve, 20);
    log.put(&reject).unwrap();
    log.put(&approve).unwrap();
    let before = log.arrival_seq(&reject.attestation_id).unwrap();
    // Replay the older attestation (same id, even with a later timestamp).
    let mut replay = reject.clone();
    replay.timestamp = u64::MAX;
    log.put(&replay).unwrap();
    log.put(&reject).unwrap();
    assert_eq!(log.arrival_seq(&reject.attestation_id).unwrap(), before);
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![reject.attestation_id.clone(), approve.attestation_id.clone()],
        "a replay must not promote the older attestation"
    );
}

#[test]
fn legacy_entries_order_by_timestamp_and_below_any_stamped_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let legacy_late = review("l1", ReviewVerdict::Approve, 5_000);
    let legacy_early = review("l2", ReviewVerdict::Reject, 1_000);
    let stamped_tiny_ts = review("s", ReviewVerdict::Reject, 1);
    // Persist the legacy ones (later timestamp first), then strip their
    // arrival records so they look pre-sidecar.
    log.put(&legacy_late).unwrap();
    log.put(&legacy_early).unwrap();
    make_legacy(tmp.path(), &legacy_late);
    make_legacy(tmp.path(), &legacy_early);
    log.put(&stamped_tiny_ts).unwrap();

    assert_eq!(
        ids_in_arrival_order(&log),
        vec![
            legacy_early.attestation_id.clone(), // legacy: by timestamp
            legacy_late.attestation_id.clone(),
            stamped_tiny_ts.attestation_id.clone(), // stamped outranks both
        ]
    );
}

#[test]
fn reputting_a_legacy_entry_does_not_stamp_it() {
    // Otherwise a replay of an old Approve would jump above an old Reject.
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let old_reject = review("alice", ReviewVerdict::Reject, 1_000);
    let old_approve = review("bob", ReviewVerdict::Approve, 500);
    log.put(&old_reject).unwrap();
    log.put(&old_approve).unwrap();
    make_legacy(tmp.path(), &old_reject);
    make_legacy(tmp.path(), &old_approve);
    log.put(&old_approve).unwrap(); // replay
    assert_eq!(log.arrival_seq(&old_approve.attestation_id).unwrap(), None);
    assert_eq!(
        ids_in_arrival_order(&log),
        vec![old_approve.attestation_id.clone(), old_reject.attestation_id.clone()]
    );
}

#[test]
fn delete_removes_the_arrival_record() {
    let tmp = tempfile::tempdir().unwrap();
    let log = AttestationLog::open(tmp.path()).unwrap();
    let a = review("a", ReviewVerdict::Approve, 1);
    log.put(&a).unwrap();
    assert!(log.arrival_seq(&a.attestation_id).unwrap().is_some());
    log.delete(&a).unwrap();
    assert_eq!(log.arrival_seq(&a.attestation_id).unwrap(), None);
}

#[test]
fn concurrent_puts_get_distinct_sequence_numbers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let atts: Vec<Attestation> =
        (0..16).map(|i| review(&format!("r{i}"), ReviewVerdict::Approve, i)).collect();
    let handles: Vec<_> = atts
        .iter()
        .cloned()
        .map(|a| {
            let root = root.clone();
            std::thread::spawn(move || AttestationLog::open(&root).unwrap().put(&a).unwrap())
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let log = AttestationLog::open(&root).unwrap();
    let mut seqs: Vec<u64> =
        atts.iter().map(|a| log.arrival_seq(&a.attestation_id).unwrap().unwrap()).collect();
    seqs.sort();
    seqs.dedup();
    assert_eq!(seqs.len(), 16, "every attestation gets its own sequence number");
}
