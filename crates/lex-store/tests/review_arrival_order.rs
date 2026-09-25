//! `latest_review_verdict` (and so `promote_candidate`'s standing-Reject
//! check) orders by server-assigned arrival, not the client timestamp
//! (lex-hub M3 hardening H1).

use lex_store::Store;
use lex_vcs::{
    Attestation, AttestationKind, AttestationResult, ProducerDescriptor, ReviewVerdict,
};

const STAGE: &str = "stage-x";

/// A review attestation as a client could forge it: any reviewer, any
/// timestamp, pushed straight into the log (as `attestations/batch` does).
fn forged(reviewer: &str, verdict: ReviewVerdict, ts: u64) -> Attestation {
    let result = match verdict {
        ReviewVerdict::Approve => AttestationResult::Passed,
        ReviewVerdict::Reject => AttestationResult::Failed { detail: "x".into() },
        ReviewVerdict::RequestChanges => AttestationResult::Inconclusive { detail: "x".into() },
    };
    Attestation::with_timestamp(
        STAGE.to_string(),
        None,
        None,
        AttestationKind::Review { reviewer: reviewer.into(), verdict, notes: None },
        result,
        ProducerDescriptor { tool: "client".into(), version: "0".into(), model: None },
        None,
        ts,
    )
}

#[test]
fn a_future_dated_approve_does_not_beat_a_later_reject() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    let log = s.attestation_log().unwrap();
    // Forged Approve, timestamp far in the future, arrives first...
    log.put(&forged("mallory", ReviewVerdict::Approve, u64::MAX / 2)).unwrap();
    // ...then the owner's genuine Reject with an honest timestamp.
    s.record_review(STAGE, None, "owner", ReviewVerdict::Reject, None).unwrap();
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Reject));
}

#[test]
fn a_backdated_reject_still_wins_when_it_arrives_last() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    let log = s.attestation_log().unwrap();
    s.record_review(STAGE, None, "alice", ReviewVerdict::Approve, None).unwrap();
    log.put(&forged("bob", ReviewVerdict::Reject, 1)).unwrap();
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Reject));
}

#[test]
fn latest_verdict_is_stable_across_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let s = Store::open(tmp.path()).unwrap();
        s.attestation_log().unwrap().put(&forged("m", ReviewVerdict::Approve, u64::MAX / 2)).unwrap();
        s.record_review(STAGE, None, "owner", ReviewVerdict::Reject, None).unwrap();
    }
    let s = Store::open(tmp.path()).unwrap();
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Reject));
}

#[test]
fn replaying_an_old_approve_does_not_lift_a_standing_reject() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    let log = s.attestation_log().unwrap();
    let approve = forged("bob", ReviewVerdict::Approve, 10);
    log.put(&approve).unwrap();
    s.record_review(STAGE, None, "owner", ReviewVerdict::Reject, None).unwrap();
    // Re-put the same Approve id (even re-stamped with a later timestamp).
    let mut replay = approve.clone();
    replay.timestamp = u64::MAX / 2;
    log.put(&replay).unwrap();
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Reject));
}

#[test]
fn a_genuinely_later_approve_still_lifts_a_reject() {
    // Arrival order is not "Reject is sticky": a later Approve wins.
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    s.record_review(STAGE, None, "alice", ReviewVerdict::Reject, None).unwrap();
    s.record_review(STAGE, None, "bob", ReviewVerdict::Approve, None).unwrap();
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Approve));
}

#[test]
fn legacy_reviews_order_by_timestamp_below_stamped_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    let log = s.attestation_log().unwrap();
    let legacy_reject = forged("a", ReviewVerdict::Reject, 9_000);
    let legacy_approve = forged("b", ReviewVerdict::Approve, 1_000);
    log.put(&legacy_reject).unwrap();
    log.put(&legacy_approve).unwrap();
    for a in [&legacy_reject, &legacy_approve] {
        std::fs::remove_file(tmp.path().join("attestations/arrival").join(&a.attestation_id))
            .unwrap();
    }
    // Legacy only: later timestamp (the Reject) wins, as before.
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Reject));
    // Any stamped review outranks the legacy ones regardless of timestamps.
    log.put(&forged("c", ReviewVerdict::Approve, 1)).unwrap();
    assert_eq!(s.latest_review_verdict(STAGE).unwrap(), Some(ReviewVerdict::Approve));
}
