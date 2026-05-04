//! `lex publish` over the op-DAG model.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

#[test]
fn publish_creates_main_branch_with_head_op() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run publish");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).expect("ops field");
    assert!(ops.is_array());
    assert!(!ops.as_array().unwrap().is_empty(), "expected at least one op");
    assert!(store.path().join("branches/main.json").exists(),
        "main branch file should exist post-publish");
}

#[test]
fn republish_unchanged_source_emits_zero_ops() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src.to_str().unwrap()])
        .output().unwrap();
    let out = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).expect("ops field");
    assert_eq!(ops.as_array().unwrap().len(), 0, "expected 0 ops on no-op republish");
}

#[test]
fn blame_with_evidence_attaches_typecheck_attestation() {
    // After `lex publish`, every accepted op carries a TypeCheck::Passed
    // attestation (#132 + #147). `lex blame --with-evidence` must
    // surface that evidence under the corresponding history entry.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "blame",
            "--store", store.path().to_str().unwrap(),
            "--with-evidence",
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run blame");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").unwrap().as_array().unwrap();
    let entry = blame.iter().find(|e| e["name"] == "fac").expect("fac in blame");
    let history = entry["history"].as_array().expect("history array");
    assert!(!history.is_empty(), "expected non-empty history");
    let stage_entry = &history[0];
    let atts = stage_entry["attestations"].as_array()
        .expect("attestations field present under --with-evidence");
    assert!(!atts.is_empty(), "expected at least one TypeCheck attestation");
    assert_eq!(atts[0]["kind"]["kind"], "type_check");
    assert_eq!(atts[0]["result"]["result"], "passed");
    assert_eq!(atts[0]["produced_by"]["tool"], "lex-store");
}

#[test]
fn blame_without_evidence_does_not_attach_attestations() {
    // Without --with-evidence the JSON shape stays unchanged
    // (no `attestations` field). Important for backward
    // compatibility with consumers that didn't ask for evidence.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "blame",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run blame");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").unwrap().as_array().unwrap();
    let entry = blame.iter().find(|e| e["name"] == "fac").unwrap();
    let history = entry["history"].as_array().unwrap();
    assert!(
        history[0].get("attestations").is_none(),
        "attestations field must not appear without --with-evidence",
    );
}

#[test]
fn blame_after_rename_shows_one_causal_event() {
    let store = tempdir().unwrap();
    let src1 = store.path().join("a.lex");
    std::fs::write(&src1, "fn parse(s :: Str) -> Int { 0 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();
    // Rename: same body, new name.
    std::fs::write(&src1, "fn parse_int(s :: Str) -> Int { 0 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args(["--output","json","blame","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").or_else(|| v.get("blame")).unwrap();
    let parse_int = blame.as_array().unwrap().iter()
        .find(|e| e["name"] == "parse_int").expect("parse_int in blame");
    let causal = parse_int["causal_history"].as_array().expect("causal_history");
    let renames: Vec<_> = causal.iter()
        .filter(|e| e["kind"] == "rename_symbol").collect();
    assert_eq!(renames.len(), 1, "expected exactly one rename in causal history");
}
