//! `lex agent-tool --store` — Examples / Spec / DiffBody / SandboxRun
//! attestation producer (#132 final producer slice).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

const ID_BODY: &str = "input";

const ID_EXAMPLES: &str = r#"[
    {"input": "a", "expected": "a"},
    {"input": "b", "expected": "b"}
]"#;

const ID_BAD_EXAMPLES: &str = r#"[
    {"input": "a", "expected": "z"}
]"#;

fn list_attestations(store: &std::path::Path) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "attest", "filter",
            "--store", store.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "attest filter failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn run_agent_tool(store: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<String> = vec![
        "agent-tool".into(),
        "--allow-effects".into(), "".into(),
        "--quiet".into(),
        "--body".into(), ID_BODY.into(),
        "--input".into(), "hello".into(),
        "--store".into(), store.to_str().unwrap().to_string(),
    ];
    for e in extra { args.push((*e).to_string()); }
    Command::new(lex_bin())
        .args(&args)
        .output()
        .expect("run agent-tool")
}

#[test]
fn passing_examples_writes_examples_attestation() {
    let store = tempdir().unwrap();
    let ex = store.path().join("ex.json");
    std::fs::write(&ex, ID_EXAMPLES).unwrap();

    let out = run_agent_tool(store.path(), &[
        "--examples", ex.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "agent-tool failed: {}", String::from_utf8_lossy(&out.stderr));

    let v = list_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let examples_att = atts.iter().find(|a| a["kind"]["kind"] == "examples")
        .expect("expected an Examples attestation");
    assert_eq!(examples_att["result"]["result"], "passed");
    assert_eq!(examples_att["kind"]["count"], 2);
    assert_eq!(examples_att["produced_by"]["tool"], "lex agent-tool");
    let file_hash = examples_att["kind"]["file_hash"].as_str().unwrap();
    assert_eq!(file_hash.len(), 64, "file_hash should be SHA-256 hex");

    // The successful single-shot run also produces a SandboxRun.
    let sandbox_att = atts.iter().find(|a| a["kind"]["kind"] == "sandbox_run")
        .expect("expected a SandboxRun attestation");
    assert_eq!(sandbox_att["result"]["result"], "passed");
}

#[test]
fn failing_examples_writes_failed_attestation() {
    let store = tempdir().unwrap();
    let ex = store.path().join("bad.json");
    std::fs::write(&ex, ID_BAD_EXAMPLES).unwrap();

    let out = run_agent_tool(store.path(), &[
        "--examples", ex.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(5), "expected exit 5 on examples mismatch");

    let v = list_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let examples_att = atts.iter().find(|a| a["kind"]["kind"] == "examples")
        .expect("Failed Examples attestation should still persist");
    assert_eq!(examples_att["result"]["result"], "failed");
    let detail = examples_att["result"]["detail"].as_str().unwrap_or("");
    assert!(detail.contains("mismatched"), "detail: {detail}");

    // No SandboxRun on failure: examples block exits before the
    // single-shot run.
    assert!(
        atts.iter().all(|a| a["kind"]["kind"] != "sandbox_run"),
        "no SandboxRun expected when examples failed",
    );
}

#[test]
fn diff_body_passes_writes_diff_body_attestation() {
    // Same body twice → no divergence → Passed.
    let store = tempdir().unwrap();
    let out = run_agent_tool(store.path(), &[
        "--diff-body", ID_BODY,
    ]);
    assert!(out.status.success(), "agent-tool failed: {}", String::from_utf8_lossy(&out.stderr));

    let v = list_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let diff_att = atts.iter().find(|a| a["kind"]["kind"] == "diff_body")
        .expect("expected a DiffBody attestation");
    assert_eq!(diff_att["result"]["result"], "passed");
    let other_hash = diff_att["kind"]["other_body_hash"].as_str().unwrap();
    assert_eq!(other_hash.len(), 64);
    assert_eq!(diff_att["kind"]["input_count"], 1);
}

#[test]
fn diff_body_diverges_writes_failed_attestation() {
    // Different body that returns "constant" instead of input.
    let store = tempdir().unwrap();
    let out = run_agent_tool(store.path(), &[
        "--diff-body", "\"constant\"",
    ]);
    assert_eq!(out.status.code(), Some(7), "expected exit 7 on divergence");

    let v = list_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let diff_att = atts.iter().find(|a| a["kind"]["kind"] == "diff_body")
        .expect("expected a DiffBody attestation on divergence too");
    assert_eq!(diff_att["result"]["result"], "failed");
    let detail = diff_att["result"]["detail"].as_str().unwrap_or("");
    assert!(detail.contains("diverged"), "detail: {detail}");
}

#[test]
fn no_store_means_no_attestation() {
    // Without --store, agent-tool runs as before — no attestation
    // directory created.
    let store = tempdir().unwrap();
    let ex = store.path().join("ex.json");
    std::fs::write(&ex, ID_EXAMPLES).unwrap();
    let out = Command::new(lex_bin())
        .args([
            "agent-tool",
            "--allow-effects", "",
            "--quiet",
            "--body", ID_BODY,
            "--input", "hello",
            "--examples", ex.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "agent-tool failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!store.path().join("attestations").exists());
}

#[test]
fn sandbox_run_records_allowed_effects() {
    // No examples, no spec — just a single-shot run. Should still
    // produce a SandboxRun attestation tagged with the effect set
    // the policy allowed.
    let store = tempdir().unwrap();
    let out = run_agent_tool(store.path(), &[]);
    assert!(out.status.success());

    let v = list_attestations(store.path());
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    let sandbox_att = atts.iter().find(|a| a["kind"]["kind"] == "sandbox_run")
        .expect("expected a SandboxRun attestation");
    assert_eq!(sandbox_att["result"]["result"], "passed");
    let effects = sandbox_att["kind"]["effects"].as_array().unwrap();
    // Empty allow-effects means no effects in the attestation.
    assert_eq!(effects.len(), 0);
}
