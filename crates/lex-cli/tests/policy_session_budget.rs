//! Conformance for `lex policy session-budget` (#292 slice 2).
//! Drives the CLI as a subprocess against a fresh store and
//! checks the resulting `policy.json`.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

fn run(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(lex_bin()).args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn read_policy(root: &std::path::Path) -> serde_json::Value {
    let bytes = std::fs::read(root.join("policy.json")).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn set_default_cap_persists_in_policy_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, stdout, stderr) = run(&[
        "policy", "session-budget", "set-default", "5000",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(ok, "stderr={stderr}\nstdout={stdout}");
    let policy = read_policy(tmp.path());
    assert_eq!(policy["session_budgets"]["default_cap"], 5000);
}

#[test]
fn set_per_session_override_persists() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, _, stderr) = run(&[
        "policy", "session-budget", "set", "ses_alpha", "8000",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(ok, "stderr={stderr}");
    let policy = read_policy(tmp.path());
    assert_eq!(policy["session_budgets"]["overrides"]["ses_alpha"], 8000);
}

#[test]
fn unbounded_override_writes_null() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, _, _) = run(&[
        "policy", "session-budget", "unbounded", "ses_human",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let policy = read_policy(tmp.path());
    assert!(policy["session_budgets"]["overrides"]["ses_human"].is_null(),
        "explicit-null override must serialize as null; got {:?}",
        policy["session_budgets"]["overrides"]["ses_human"]);
}

#[test]
fn clear_removes_the_override() {
    let tmp = tempfile::tempdir().unwrap();
    run(&[
        "policy", "session-budget", "set", "ses_alpha", "8000",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    let (ok, _, _) = run(&[
        "policy", "session-budget", "clear", "ses_alpha",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let policy = read_policy(tmp.path());
    assert!(policy["session_budgets"]["overrides"].as_object()
        .map(|m| m.is_empty()).unwrap_or(true),
        "override should be removed; got {:?}",
        policy["session_budgets"]["overrides"]);
}

#[test]
fn unknown_subcommand_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, _, stderr) = run(&[
        "policy", "session-budget", "nuke-everything",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(stderr.contains("unknown `session-budget` subcommand"),
        "expected unknown-subcommand message, got: {stderr}");
}
