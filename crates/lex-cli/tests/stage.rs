//! `lex stage <stage_id>` and `lex stage <stage_id> --attestations`.

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
        .output()
        .expect("run publish");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn lex_stage_prints_metadata_and_ast() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let v = publish(store.path(), &src);
    let ops = v.pointer("/data/ops").unwrap().as_array().unwrap();
    let stage_id = ops[0]["kind"]["stage_id"].as_str().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "stage",
            "--store", store.path().to_str().unwrap(),
            stage_id,
        ])
        .output()
        .expect("run stage");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let data = v.pointer("/data").unwrap();
    assert!(data["metadata"]["stage_id"].as_str().is_some());
    assert!(data["ast"].is_object());
    assert!(data["status"].is_string());
}

#[test]
fn lex_stage_attestations_lists_typecheck_evidence() {
    // After a `lex publish` against a fresh store, the store-write
    // gate (#130) emits a TypeCheck::Passed attestation. The CLI
    // must surface it via `lex stage <id> --attestations`.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let v = publish(store.path(), &src);
    let ops = v.pointer("/data/ops").unwrap().as_array().unwrap();
    let stage_id = ops[0]["kind"]["stage_id"].as_str().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "stage",
            "--store", store.path().to_str().unwrap(),
            stage_id,
            "--attestations",
        ])
        .output()
        .expect("run stage --attestations");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    assert!(!atts.is_empty(), "expected a TypeCheck attestation; got: {v}");
    assert_eq!(atts[0]["kind"]["kind"], "type_check");
    assert_eq!(atts[0]["result"]["result"], "passed");
    assert_eq!(atts[0]["produced_by"]["tool"], "lex-store");
    assert_eq!(atts[0]["stage_id"], stage_id);
}

#[test]
fn lex_stage_unknown_id_errors() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "stage",
            "--store", store.path().to_str().unwrap(),
            "nonexistent_stage_id",
            "--attestations",
        ])
        .output()
        .expect("run stage");
    assert!(!out.status.success(), "expected nonzero exit on unknown stage");
}
