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
