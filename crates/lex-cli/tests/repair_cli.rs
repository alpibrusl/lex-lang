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
fn repair_apply_without_transform_errors() {
    // Slice 2a: `--apply` requires `--transform '<json>'`. The
    // LLM-driven path (no `--transform`) ships in slice 2b.
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["repair", "fake-op-id", "--apply",
            "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(!out.status.success(),
        "--apply without --transform should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("requires `--transform"),
        "expected '--apply requires --transform' message, got: {stderr}");
}

#[test]
fn repair_transform_without_apply_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["repair", "fake-op-id", "--transform", "{}",
            "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(!out.status.success(),
        "--transform without --apply should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--transform` requires `--apply"),
        "expected message, got: {stderr}");
}

#[test]
fn repair_apply_without_hint_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["repair", "fake-op-id", "--apply",
            "--transform", r#"{"kind":"inline_let","from_stage_id":"x","let_node":"n_0.1"}"#,
            "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(!out.status.success(),
        "applying without a matching RepairHint should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no RepairHint exists"),
        "expected hint-required message, got: {stderr}");
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
