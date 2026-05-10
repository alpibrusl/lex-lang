//! Conformance tests for #282 — `lex docs --for-agent`.
//!
//! These tests drive the `lex` binary as a subprocess to check the
//! end-to-end JSON envelope. They use `--store` to point at a
//! freshly-prepared temp directory so no global state is touched.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    // `cargo test --workspace` builds the lex binary at
    // target/debug/lex; CARGO_BIN_EXE_lex resolves to it
    // automatically when the bin is named `lex` in lex-cli.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

fn seed_store(tmp: &std::path::Path) {
    // Use the CLI's `publish` command to seed a tiny stage,
    // exercising the full pipeline. Simpler than constructing
    // Store manually because the test crate is a `lex-cli` integ
    // test (no direct dependency on `lex-store`).
    let src = tmp.join("hello.lex");
    std::fs::write(&src,
        "fn hello(n :: Int) -> Int { match n { 0 => 1, _ => n } }\n",
    ).unwrap();
    let out = Command::new(lex_bin())
        .args(["publish", src.to_str().unwrap(), "--store", tmp.to_str().unwrap()])
        .output()
        .expect("lex publish should run");
    if !out.status.success() {
        panic!(
            "publish failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

fn run_docs(tmp: &std::path::Path) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args(["--output", "json", "docs", "--for-agent",
            "--store", tmp.to_str().unwrap()])
        .output()
        .expect("lex docs should run");
    assert!(out.status.success(),
        "lex docs --for-agent failed: stderr={}",
        String::from_utf8_lossy(&out.stderr));
    let envelope: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\nbody={}",
            String::from_utf8_lossy(&out.stdout)));
    // The acli wrapper nests our envelope under `data`. Unwrap so
    // tests can address sections directly.
    envelope["data"].clone()
}

#[test]
fn emits_versioned_envelope_with_all_required_sections() {
    let tmp = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let v = run_docs(tmp.path());
    assert_eq!(v["lex_docs_version"], 1);
    assert!(v["workspace"].is_object(), "workspace section present");
    assert!(v["stdlib"]["sigs"].is_array(), "stdlib.sigs present");
    assert!(v["recent_activity"].is_array(), "recent_activity present");
    assert!(v["open_intents"].is_array(), "open_intents present");
    assert!(v["policy"].is_object(), "policy section present");
    assert!(v["attention"].is_array(), "attention queue present");
}

#[test]
fn workspace_section_reports_branch_and_version() {
    let tmp = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let v = run_docs(tmp.path());
    let ws = &v["workspace"];
    assert!(ws["lex_version"].is_string());
    assert_eq!(ws["current_branch"], "main");
    assert_eq!(ws["default_branch"], "main");
    let branches = ws["branches"].as_array().expect("branches array");
    assert!(branches.iter().any(|b| b == "main"));
}

#[test]
fn stdlib_sigs_include_published_fn_with_signature() {
    let tmp = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let v = run_docs(tmp.path());
    let sigs = v["stdlib"]["sigs"].as_array().expect("sigs is an array");
    let hello = sigs.iter().find(|s| s["name"] == "hello")
        .expect("published `hello` should appear in stdlib summary");
    assert!(hello["sig_id"].is_string());
    assert!(hello["stage_id"].is_string());
    assert!(hello["type_signature"].as_str().unwrap().contains("Int"),
        "type signature should mention Int, got {}",
        hello["type_signature"]);
}

#[test]
fn recent_activity_lists_publish_ops() {
    let tmp = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let v = run_docs(tmp.path());
    let recent = v["recent_activity"].as_array().unwrap();
    assert!(!recent.is_empty(), "publish should land at least one op");
    // First op is the most recent — should reference a fn add.
    let first = &recent[0];
    assert!(first["op_id"].is_string());
    assert!(first["kind_tag"].is_string());
}

#[test]
fn limit_recent_bounds_the_history() {
    let tmp = tempfile::tempdir().unwrap();
    seed_store(tmp.path());

    let out = Command::new(lex_bin())
        .args(["--output", "json", "docs", "--for-agent",
            "--limit-recent", "0",
            "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let v = &envelope["data"];
    assert_eq!(v["recent_activity"].as_array().unwrap().len(), 0,
        "--limit-recent 0 should yield empty activity");
}

#[test]
fn unknown_subcommand_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args(["docs", "--no-such-flag", "--store", tmp.path().to_str().unwrap()])
        .output().unwrap();
    assert!(!out.status.success(), "unknown flag should fail the command");
}

#[test]
fn fresh_store_emits_empty_recent_activity() {
    let tmp = tempfile::tempdir().unwrap();
    // Don't publish anything — fresh store. The command must still
    // succeed and emit an envelope with empty `recent_activity` /
    // `open_intents` / `attention` lists.
    let v = run_docs(tmp.path());
    assert_eq!(v["lex_docs_version"], 1);
    assert!(v["recent_activity"].as_array().unwrap().is_empty());
    assert!(v["open_intents"].as_array().unwrap().is_empty());
    assert!(v["attention"].as_array().unwrap().is_empty());
}
