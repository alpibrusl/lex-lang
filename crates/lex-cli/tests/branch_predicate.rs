//! `lex branch create <name> --predicate '<json>'` — predicate-defined
//! branches (#133).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn read_branch_file(store: &std::path::Path, name: &str) -> serde_json::Value {
    let path = store.join("branches").join(format!("{name}.json"));
    let bytes = std::fs::read(&path).expect("read branch file");
    serde_json::from_slice(&bytes).expect("parse branch file")
}

#[test]
fn create_predicate_branch_persists_predicate_field() {
    let store = tempdir().unwrap();
    let pred = r#"{"predicate":"all"}"#;
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "branch", "create",
            "--store", store.path().to_str().unwrap(),
            "all_ops",
            "--predicate", pred,
        ])
        .output().unwrap();
    assert!(out.status.success(), "create: {}", String::from_utf8_lossy(&out.stderr));

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/created").unwrap(), "all_ops");
    assert_eq!(v.pointer("/data/predicate/predicate").unwrap(), "all");

    let bf = read_branch_file(store.path(), "all_ops");
    assert_eq!(bf["name"], "all_ops");
    assert!(bf["head_op"].is_null(), "predicate branch starts with head_op = None");
    assert_eq!(bf["predicate"]["predicate"], "all");
}

#[test]
fn create_predicate_branch_with_compound_clause() {
    let store = tempdir().unwrap();
    let pred = r#"{"predicate":"and","clauses":[{"predicate":"all"},{"predicate":"intent","intent_id":"int-123"}]}"#;
    let out = Command::new(lex_bin())
        .args([
            "branch", "create",
            "--store", store.path().to_str().unwrap(),
            "view",
            "--predicate", pred,
        ])
        .output().unwrap();
    assert!(out.status.success(), "create: {}", String::from_utf8_lossy(&out.stderr));
    let bf = read_branch_file(store.path(), "view");
    assert_eq!(bf["predicate"]["predicate"], "and");
    let clauses = bf["predicate"]["clauses"].as_array().unwrap();
    assert_eq!(clauses.len(), 2);
}

#[test]
fn create_predicate_branch_rejects_invalid_json() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "branch", "create",
            "--store", store.path().to_str().unwrap(),
            "broken",
            "--predicate", "{not even json",
        ])
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("predicate") && stderr.contains("parse"),
        "expected parse error in stderr: {stderr}");
}

#[test]
fn create_predicate_branch_rejects_unknown_predicate_kind() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "branch", "create",
            "--store", store.path().to_str().unwrap(),
            "broken",
            "--predicate", r#"{"predicate":"nonsense"}"#,
        ])
        .output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn create_branch_without_predicate_still_snapshots_from_parent() {
    // Backwards-compat: existing flag-less invocations stay
    // snapshot-style.
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "branch", "create",
            "--store", store.path().to_str().unwrap(),
            "feature",
        ])
        .output().unwrap();
    assert!(out.status.success(), "create: {}", String::from_utf8_lossy(&out.stderr));
    let bf = read_branch_file(store.path(), "feature");
    assert_eq!(bf["name"], "feature");
    assert!(bf.get("predicate").is_none() || bf["predicate"].is_null(),
        "snapshot branch should not carry a predicate");
    assert_eq!(bf["parent"], "main");
}
