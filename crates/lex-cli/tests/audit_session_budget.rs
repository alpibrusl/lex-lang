//! Conformance for `lex audit --budget --by-session` (#292 slice 1).
//! Drives the CLI as a subprocess against a seeded store.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

fn run_json(args: &[&str]) -> (bool, serde_json::Value) {
    let out = Command::new(lex_bin()).args(args).output().unwrap();
    let env: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!(
            "expected JSON on stdout, got: {}\nstderr={}\nerr={e}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ));
    (out.status.success(), env)
}

#[test]
fn budget_by_session_on_empty_store_emits_empty_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, env) = run_json(&[
        "--output", "json",
        "audit", "--budget", "--by-session",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let v = &env["data"];
    assert!(v["sessions"].as_array().unwrap().is_empty());
    assert_eq!(v["total_spent"], 0);
}

#[test]
fn budget_by_session_with_specific_session_pads_with_zero() {
    // When the filter session doesn't exist, the envelope still
    // emits one row with zero spend so tooling can rely on the
    // shape.
    let tmp = tempfile::tempdir().unwrap();
    let (ok, env) = run_json(&[
        "--output", "json",
        "audit", "--budget", "--session", "ses_missing",
        "--store", tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let v = &env["data"];
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_id"], "ses_missing");
    assert_eq!(sessions[0]["spent"], 0);
    assert_eq!(sessions[0]["op_count"], 0);
}

#[test]
fn by_session_or_session_without_budget_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "audit", "--by-session",
            "--store", tmp.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("require --budget"),
        "expected '--by-session require --budget' message, got: {stderr}");
}
