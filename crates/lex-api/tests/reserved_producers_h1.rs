//! lex-hub M3 hardening H1: `State::reserved_producers` keeps clients from
//! minting attestations under server-only producer names through
//! `POST /v1/attestations/batch`, and verdict ordering is by arrival.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{
    Attestation, AttestationKind, AttestationLog, AttestationResult, ProducerDescriptor,
    ReviewVerdict,
};
use serde_json::json;
use tempfile::TempDir;

fn start(tmp: &TempDir, reserved: Vec<String>) -> SocketAddr {
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(
        State::open(tmp.path().to_path_buf()).unwrap().with_reserved_producers(reserved),
    );
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    addr
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

fn att(tool: &str, stage: &str) -> Attestation {
    Attestation::with_timestamp(
        stage.to_string(),
        None,
        None,
        AttestationKind::TypeCheck,
        AttestationResult::Passed,
        ProducerDescriptor { tool: tool.into(), version: "0".into(), model: None },
        None,
        1,
    )
}

fn review_att(tool: &str, verdict: ReviewVerdict, ts: u64, stage: &str) -> Attestation {
    let result = match verdict {
        ReviewVerdict::Approve => AttestationResult::Passed,
        ReviewVerdict::Reject => AttestationResult::Failed { detail: "no".into() },
        ReviewVerdict::RequestChanges => AttestationResult::Inconclusive { detail: "x".into() },
    };
    Attestation::with_timestamp(
        stage.to_string(),
        None,
        None,
        AttestationKind::Review { reviewer: "whoever".into(), verdict, notes: None },
        result,
        ProducerDescriptor { tool: tool.into(), version: "0".into(), model: None },
        None,
        ts,
    )
}

fn batch(addr: &SocketAddr, atts: &[Attestation]) -> (u16, String) {
    http(addr, "POST", "/v1/attestations/batch", &serde_json::to_string(atts).unwrap())
}

fn log_len(tmp: &TempDir) -> usize {
    AttestationLog::open(tmp.path()).unwrap().list_all().unwrap().len()
}

const HUB: &str = lex_store::HUB_CI_PRODUCER_TOOL;

#[test]
fn reserved_hub_ci_producer_is_refused_and_nothing_is_written() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![HUB.to_string()]);
    let (status, body) = batch(&addr, &[att(HUB, "s1")]);
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("ReservedProducer") && body.contains(HUB), "{body}");
    assert_eq!(log_len(&tmp), 0, "a refused batch writes nothing");
}

#[test]
fn a_mixed_batch_is_refused_whole_in_either_order() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![HUB.to_string()]);
    let legit = att("some-ci", "s1");
    let forged = att(HUB, "s2");
    let (status, _) = batch(&addr, &[legit.clone(), forged.clone()]);
    assert_eq!(status, 403);
    let (status, _) = batch(&addr, &[forged, legit]);
    assert_eq!(status, 403);
    assert_eq!(log_len(&tmp), 0, "the legitimate half must not be written either");
}

#[test]
fn a_batch_without_reserved_producers_still_succeeds() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![HUB.to_string(), lex_store::REVIEW_PRODUCER_RESERVATION.to_string()]);
    let (status, body) = batch(&addr, &[att("some-ci", "s1"), att("another", "s2")]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(log_len(&tmp), 2);
}

#[test]
fn default_empty_reserved_set_preserves_current_behaviour() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, Vec::new());
    let (status, body) = batch(
        &addr,
        &[att(HUB, "s1"), review_att("lex-store::review:alice", ReviewVerdict::Approve, 1, "s1")],
    );
    assert_eq!(status, 200, "default must accept the same batch: {body}");
    assert_eq!(log_len(&tmp), 2);
}

#[test]
fn reserved_match_is_trimmed_and_case_insensitive() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![HUB.to_string()]);
    let (status, _) = batch(&addr, &[att(" LEX-HUB-CI ", "s1")]);
    assert_eq!(status, 403);
    assert_eq!(log_len(&tmp), 0);
}

#[test]
fn review_prefix_reservation_refuses_any_review_producer() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![lex_store::REVIEW_PRODUCER_RESERVATION.to_string()]);
    let review = review_att("lex-store::review:alice", ReviewVerdict::Approve, 1, "s1");
    let (status, body) = batch(&addr, std::slice::from_ref(&review));
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("lex-store::review:"), "{body}");
    let (status, _) = batch(&addr, &[att("some-ci", "s1"), review]);
    assert_eq!(status, 403, "mixed batch");
    // A different suffix, and the bare prefix, are covered too.
    let (status, _) = batch(&addr, &[review_att("lex-store::review:", ReviewVerdict::Reject, 1, "s1")]);
    assert_eq!(status, 403);
    assert_eq!(log_len(&tmp), 0);
    // A name that merely resembles the prefix is not caught.
    let (status, _) = batch(&addr, &[att("lex-store::reviewer-bot", "s1")]);
    assert_eq!(status, 200);
}

#[test]
fn prefix_entries_do_not_match_exact_only_entries_and_vice_versa() {
    let tmp = TempDir::new().unwrap();
    // An exact entry must not act as a prefix.
    let addr = start(&tmp, vec![HUB.to_string()]);
    let (status, _) = batch(&addr, &[att("lex-hub-ci-extra", "s1")]);
    assert_eq!(status, 200);
    // A bare `*` entry reserves everything; a blank entry reserves nothing.
    let tmp2 = TempDir::new().unwrap();
    let addr2 = start(&tmp2, vec!["".into(), "  ".into()]);
    let (status, _) = batch(&addr2, &[att("", "s1"), att("x", "s1")]);
    assert_eq!(status, 200, "blank reservations are ignored");
}

/// Publish a tiny program and return (stage_id, head_op).
fn publish(addr: &SocketAddr) -> (String, String) {
    let (s, b) = http(addr, "POST", "/v1/publish",
        &json!({"source": "fn foo(n :: Int) -> Int { n }\n", "activate": false}).to_string());
    assert_eq!(s, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let stage_id = v["ops"][0]["kind"]["stage_id"].as_str()
        .unwrap_or_else(|| panic!("no stage id in {b}")).to_string();
    let head = v["head_op"].as_str().unwrap_or_else(|| panic!("no head_op in {b}")).to_string();
    (stage_id, head)
}

#[test]
fn a_real_head_push_still_writes_the_server_side_hub_ci_attestation_when_reserved() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![HUB.to_string(), lex_store::REVIEW_PRODUCER_RESERVATION.to_string()]);
    // `target` forks from an empty `main`; then main gets a program and
    // `target`'s head is fast-forwarded onto it, which is the push path
    // that runs the hosted-CI verifier.
    let (s, b) = http(&addr, "POST", "/v1/branches", &json!({"name": "target", "from": "main"}).to_string());
    assert!(s == 200 || s == 201, "{b}");
    let (stage_id, head) = publish(&addr);
    let (s, b) = http(&addr, "POST", "/v1/branches/target/head", &json!({"head_op": head}).to_string());
    assert_eq!(s, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["ci"]["passed"], json!(true), "{b}");

    let (s, b) = http(&addr, "GET", &format!("/v1/stage/{stage_id}/attestations"), "");
    assert_eq!(s, 200, "{b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let hub = v["attestations"].as_array().unwrap().iter().any(|a| {
        a["produced_by"]["tool"] == HUB
            && a["kind"]["kind"] == json!("type_check")
            && a["result"]["result"] == json!("passed")
    });
    assert!(hub, "expected a lex-hub-ci attestation even though the name is reserved: {b}");
    // The same claim over HTTP is still refused.
    let forged = att(HUB, &stage_id);
    let (s, _) = batch(&addr, &[forged]);
    assert_eq!(s, 403);
}

#[test]
fn the_verdict_route_still_writes_review_attestations_when_the_prefix_is_reserved() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, vec![lex_store::REVIEW_PRODUCER_RESERVATION.to_string()]);
    let (stage_id, _) = publish(&addr);
    let (s, b) = http(&addr, "POST", "/v1/review/verdict", &json!({
        "stage_id": stage_id, "verdict": "reject", "reviewer": "alice"}).to_string());
    assert_eq!(s, 201, "{b}");
    let log = AttestationLog::open(tmp.path()).unwrap();
    let all = log.list_for_stage(&stage_id).unwrap();
    assert!(
        all.iter().any(|a| a.produced_by.tool == "lex-store::review:alice"
            && matches!(a.kind, AttestationKind::Review { .. })),
        "server-side record_review must bypass the reservation: {all:?}"
    );
}

#[test]
fn a_future_dated_approve_via_batch_does_not_beat_a_later_reject() {
    let tmp = TempDir::new().unwrap();
    let addr = start(&tmp, Vec::new());
    let (stage_id, _) = publish(&addr);
    // Forged Approve, far-future timestamp, arrives first via the batch route.
    let (s, b) = batch(&addr, &[review_att("client", ReviewVerdict::Approve, u64::MAX / 2, &stage_id)]);
    assert_eq!(s, 200, "{b}");
    // Owner's Reject arrives later through the verdict route.
    let (s, b) = http(&addr, "POST", "/v1/review/verdict", &json!({
        "stage_id": stage_id, "verdict": "reject", "reviewer": "owner"}).to_string());
    assert_eq!(s, 201, "{b}");
    let store = lex_store::Store::open(tmp.path()).unwrap();
    assert_eq!(store.latest_review_verdict(&stage_id).unwrap(), Some(ReviewVerdict::Reject));
}
