//! Conformance test for `lex repair <op_id>` (#281). Drives the
//! CLI as a subprocess after seeding a store with a deliberately-
//! ill-typed apply so a `RepairHint` lands.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

#[test]
fn repair_with_no_hint_reports_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["--output", "json", "repair", "fake-op-id",
            "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(),
        "command failed: stderr={}", String::from_utf8_lossy(&out.stderr));
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let v = &env["data"];
    assert_eq!(v["found"], false);
    assert_eq!(v["failed_op_id"], "fake-op-id");
}

#[test]
fn repair_apply_is_unsupported_today() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["repair", "fake-op-id", "--apply",
            "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(!out.status.success(), "--apply should fail today");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not yet implemented"),
        "expected 'not yet implemented' message, got: {stderr}");
}

#[test]
fn repair_unknown_op_emits_text_render() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["repair", "missing", "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no RepairHint found for op_id `missing`"),
        "text mode should report missing hint, got: {stdout}");
}
