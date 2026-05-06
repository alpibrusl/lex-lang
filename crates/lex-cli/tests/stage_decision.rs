//! `lex stage defer|block|unblock` — human triage actions (lex-tea v3b, #172).
//! Records a Defer/Block/Unblock attestation. Block additionally
//! makes `lex stage pin` refuse until an Unblock supersedes it.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, src: &std::path::Path) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "publish failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn first_stage_id(publish_out: &serde_json::Value) -> String {
    publish_out.pointer("/data/ops/0/kind/stage_id")
        .and_then(|v| v.as_str())
        .expect("stage_id in publish output")
        .to_string()
}

fn run_decision(verb: &str, store: &std::path::Path, stage_id: &str, reason: &str, actor: &str)
    -> std::process::Output
{
    Command::new(lex_bin())
        .args([
            "--output", "json",
            "stage", verb, stage_id,
            "--reason", reason,
            "--actor", actor,
            "--store", store.to_str().unwrap(),
        ])
        .output().unwrap()
}

fn count_of_kind(store: &std::path::Path, kind: &str) -> u64 {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "attest", "filter",
            "--store", store.to_str().unwrap(),
            "--kind", kind,
        ])
        .output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v.pointer("/data/count").unwrap().as_u64().unwrap()
}

#[test]
fn stage_defer_records_defer_attestation() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = run_decision("defer", store.path(), &stage_id, "revisit next sprint", "alice");
    assert!(out.status.success(), "defer failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/decision").unwrap(), "defer");
    assert_eq!(v.pointer("/data/actor").unwrap(), "alice");

    assert_eq!(count_of_kind(store.path(), "defer"), 1);
}

#[test]
fn stage_block_then_pin_refuses() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = run_decision("block", store.path(), &stage_id, "spec wrong", "alice");
    assert!(out.status.success(), "block failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(count_of_kind(store.path(), "block"), 1);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "ship anyway",
            "--actor", "bob",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success(), "pin should refuse a blocked stage");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("blocked"), "stderr should mention blocked: {stderr}");
}

#[test]
fn stage_unblock_clears_block() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    assert!(run_decision("block", store.path(), &stage_id, "wait for review", "alice")
        .status.success());
    // Sleep one second so the unblock has a strictly later timestamp
    // than the block — the resolver picks "latest" by timestamp.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert!(run_decision("unblock", store.path(), &stage_id, "review done", "alice")
        .status.success());

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "ship",
            "--actor", "bob",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(),
        "pin should succeed after unblock: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn stage_decision_requires_reason() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    for verb in ["defer", "block", "unblock"] {
        let out = Command::new(lex_bin())
            .args([
                "stage", verb, &stage_id,
                "--actor", "alice",
                "--store", store.path().to_str().unwrap(),
            ])
            .output().unwrap();
        assert!(!out.status.success(), "{verb} without --reason should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("reason"),
            "{verb} stderr should mention reason: {stderr}");
    }
}

#[test]
fn stage_decision_requires_actor() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    for verb in ["defer", "block", "unblock"] {
        let out = Command::new(lex_bin())
            .args([
                "stage", verb, &stage_id,
                "--reason", "x",
                "--store", store.path().to_str().unwrap(),
            ])
            .env_remove("LEX_TEA_USER")
            .output().unwrap();
        assert!(!out.status.success(), "{verb} without actor should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("actor"),
            "{verb} stderr should mention actor: {stderr}");
    }
}

#[test]
fn stage_decision_fails_on_unknown_stage() {
    let store = tempdir().unwrap();
    for verb in ["defer", "block", "unblock"] {
        let out = run_decision(verb, store.path(), "no_such_stage", "x", "alice");
        assert!(!out.status.success(), "{verb} on unknown stage should fail");
    }
}
