//! `lex op pull` syncs attestations, not just ops/stages/intents (#916).
//!
//! Regression for the gap found in prod: the hosted CI runner (#910) writes a
//! trusted `lex-hub-ci` TypeCheck attestation server-side when a push advances
//! a branch head, but `lex op pull` used to fetch only ops + stages + intents,
//! so a puller never saw those verdicts. This drives the real binary against a
//! real `lex-api` server end to end:
//!
//!   author store --op push--> server (writes lex-hub-ci attestation on the
//!   head-advance) --op pull--> consumer store, which must now carry the
//!   attestation.

use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

struct Server {
    addr: SocketAddr,
    tmp: TempDir,
}

fn start_server() -> Server {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(50));
    Server { addr, tmp }
}

fn run(args: &[&str]) -> std::process::Output {
    let out = Command::new(lex_bin()).args(args).output().unwrap();
    if !out.status.success() {
        eprintln!(
            "`lex {}` failed:\n  stdout: {}\n  stderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out
}

#[test]
fn op_pull_syncs_hosted_ci_attestations() {
    let srv = start_server();
    let url = format!("http://{}", srv.addr);

    let tmp = TempDir::new().unwrap();
    let author = tmp.path().join("author");
    let consumer = tmp.path().join("consumer");
    let src = tmp.path().join("lib.lex");
    std::fs::write(&src, "fn double(x :: Int) -> Int { x * 2 }\n").unwrap();

    // Author publishes locally, then pushes. The push's branch-head advance
    // makes the server re-type-check and write a `lex-hub-ci` attestation.
    let out = run(&[
        "publish", "--store", author.to_str().unwrap(), "--branch", "main",
        "--activate", src.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "publish must succeed");

    let out = run(&["op", "push", &url, "--store", author.to_str().unwrap()]);
    assert!(out.status.success(), "push must succeed");

    // A fresh consumer pulls. The pulled store must now carry the attestation.
    let out = run(&["op", "pull", &url, "--store", consumer.to_str().unwrap()]);
    assert!(out.status.success(), "pull must succeed");
    let pull_stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        pull_stdout.contains("attestation"),
        "pull should report attestations synced: {pull_stdout}"
    );

    // The trusted server-side verdict is readable in the consumer store.
    let out = run(&["attest", "filter", "--store", consumer.to_str().unwrap(), "--kind", "type_check"]);
    assert!(out.status.success(), "attest filter must succeed");
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        listing.contains("lex-hub-ci") && listing.contains("type_check") && listing.contains("passed"),
        "pulled store must carry the hosted-CI TypeCheck attestation, got:\n{listing}"
    );
}

/// H1 (lex-hub M3 hardening): a pulled store keeps the remote's ARRIVAL order
/// of review verdicts (each pulled attestation gets a local stamp at pull
/// time), so a forged future-dated Approve that arrived before the owner's
/// Reject on the remote does not come out on top locally.
#[test]
fn op_pull_preserves_remote_arrival_order_of_review_verdicts() {
    use lex_vcs::{Attestation, AttestationKind, AttestationLog, AttestationResult, ProducerDescriptor, ReviewVerdict};

    let srv = start_server();
    let url = format!("http://{}", srv.addr);
    let tmp = TempDir::new().unwrap();
    let author = tmp.path().join("author");
    let consumer = tmp.path().join("consumer");
    let src = tmp.path().join("lib.lex");
    std::fs::write(&src, "fn triple(x :: Int) -> Int { x * 3 }\n").unwrap();
    assert!(run(&["publish", "--store", author.to_str().unwrap(), "--branch", "main", "--activate", src.to_str().unwrap()]).status.success());
    assert!(run(&["op", "push", &url, "--store", author.to_str().unwrap()]).status.success());

    // The server's store: find the pushed stage, then append two verdicts
    // in a known arrival order with timestamps that disagree with it.
    let server_log = AttestationLog::open(srv.tmp.path()).unwrap();
    let stage_id = server_log.list_all().unwrap().first().expect("hub-ci attestation").stage_id.clone();
    let verdict = |v: ReviewVerdict, who: &str, ts: u64| {
        let result = match v {
            ReviewVerdict::Approve => AttestationResult::Passed,
            _ => AttestationResult::Failed { detail: "no".into() },
        };
        Attestation::with_timestamp(
            stage_id.clone(), None, None,
            AttestationKind::Review { reviewer: who.into(), verdict: v, notes: None },
            result,
            ProducerDescriptor { tool: format!("t:{who}"), version: "0".into(), model: None },
            None, ts,
        )
    };
    server_log.put(&verdict(ReviewVerdict::Approve, "mallory", u64::MAX / 2)).unwrap();
    server_log.put(&verdict(ReviewVerdict::Reject, "owner", 1)).unwrap();

    assert!(run(&["op", "pull", &url, "--store", consumer.to_str().unwrap()]).status.success());
    let store = lex_store::Store::open(&consumer).unwrap();
    assert_eq!(
        store.latest_review_verdict(&stage_id).unwrap(),
        Some(ReviewVerdict::Reject),
        "pull must keep the remote's arrival order, not re-derive it from timestamps"
    );
    let clog = store.attestation_log().unwrap();
    for a in clog.list_for_stage(&stage_id).unwrap() {
        assert!(
            clog.arrival_seq(&a.attestation_id).unwrap().is_some(),
            "pulled attestation must carry a LOCAL arrival stamp"
        );
    }
}
