//! `lex stage pin` — human override action (lex-tea v3a, #172).
//! Activates the stage and records an `Override` attestation
//! that downstream `lex attest filter --kind override` can find.

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

#[test]
fn stage_pin_records_override_attestation() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "stage", "pin", &stage_id,
            "--reason", "spec checker is wrong here",
            "--actor", "alice",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "pin failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/pinned").unwrap(), &stage_id);
    assert_eq!(v.pointer("/data/actor").unwrap(), "alice");
    assert_eq!(v.pointer("/data/reason").unwrap(), "spec checker is wrong here");

    // The Override attestation should now be queryable via attest
    // filter and show up alongside the auto-emitted TypeCheck.
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "attest", "filter",
            "--store", store.path().to_str().unwrap(),
            "--kind", "override",
        ])
        .output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let count = v.pointer("/data/count").unwrap().as_u64().unwrap();
    assert_eq!(count, 1, "expected exactly one Override attestation");
    let att = v.pointer("/data/attestations/0").unwrap();
    assert_eq!(att["kind"]["kind"], "override");
    assert_eq!(att["kind"]["actor"], "alice");
    assert_eq!(att["kind"]["reason"], "spec checker is wrong here");
    assert_eq!(att["produced_by"]["tool"], "lex stage pin");
    assert_eq!(att["stage_id"], stage_id);
}

#[test]
fn stage_pin_uses_lex_tea_user_env_when_actor_omitted() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "policy override",
            "--store", store.path().to_str().unwrap(),
        ])
        .env("LEX_TEA_USER", "bob-from-env")
        .output().unwrap();
    assert!(out.status.success(), "pin failed: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn stage_pin_fails_without_actor_or_env() {
    // No --actor and no LEX_TEA_USER → refuse. Anonymous overrides
    // would defeat the audit story.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "x",
            "--store", store.path().to_str().unwrap(),
        ])
        .env_remove("LEX_TEA_USER")
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("actor"), "stderr should mention actor: {stderr}");
}

#[test]
fn stage_pin_requires_reason() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--actor", "alice",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reason"), "stderr should mention reason: {stderr}");
}

#[test]
fn stage_pin_fails_on_unknown_stage() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", "no_such_stage",
            "--reason", "x",
            "--actor", "alice",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success());
}
