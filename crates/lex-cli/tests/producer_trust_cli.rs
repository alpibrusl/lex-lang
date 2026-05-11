//! Conformance for `lex producer-trust recompute` (#293).

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

#[test]
fn recompute_on_empty_store_reports_no_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "--output", "json", "producer-trust", "recompute",
            "--tool", "test-tool",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(),
        "stderr={}", String::from_utf8_lossy(&out.stderr));
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["ok"], false);
    assert!(env["data"]["reason"].as_str().unwrap().contains("no attestations"));
}

#[test]
fn recompute_requires_tool_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "producer-trust", "recompute",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--tool"),
        "expected --tool requirement, got: {stderr}");
}

#[test]
fn unknown_subcommand_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "producer-trust", "delete-all",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown"),
        "expected unknown-subcommand message, got: {stderr}");
}
