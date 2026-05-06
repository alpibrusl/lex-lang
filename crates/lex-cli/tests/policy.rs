//! `lex policy {block-producer|unblock-producer|list}` — local
//! trust policy at `<store>/policy.json` (#181).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn run(args: &[&str]) -> std::process::Output {
    Command::new(lex_bin()).args(args).output().unwrap()
}

#[test]
fn block_producer_creates_policy_json() {
    let store = tempdir().unwrap();
    let out = run(&[
        "--output", "json",
        "policy", "block-producer", "buggy-bot",
        "--reason", "false positives",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(), "block: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/tool").unwrap(), "buggy-bot");
    assert_eq!(v.pointer("/data/newly_blocked").unwrap(), true);

    // policy.json should now exist on disk.
    let policy_path = store.path().join("policy.json");
    assert!(policy_path.exists(), "policy.json should be created");

    // Re-blocking the same tool is a no-op.
    let out = run(&[
        "--output", "json",
        "policy", "block-producer", "buggy-bot",
        "--reason", "still buggy",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/newly_blocked").unwrap(), false);
}

#[test]
fn unblock_producer_removes_entry() {
    let store = tempdir().unwrap();
    assert!(run(&[
        "policy", "block-producer", "bot",
        "--reason", "x",
        "--store", store.path().to_str().unwrap(),
    ]).status.success());

    let out = run(&[
        "--output", "json",
        "policy", "unblock-producer", "bot",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(), "unblock: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/was_blocked").unwrap(), true);

    // Listing should now be empty.
    let out = run(&[
        "--output", "json",
        "policy", "list",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/count").unwrap(), 0);
}

#[test]
fn list_shows_all_blocked_producers() {
    let store = tempdir().unwrap();
    for (tool, reason) in [("a", "r1"), ("b", "r2"), ("c", "r3")] {
        assert!(run(&[
            "policy", "block-producer", tool,
            "--reason", reason,
            "--store", store.path().to_str().unwrap(),
        ]).status.success());
    }
    let out = run(&[
        "--output", "json",
        "policy", "list",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/count").unwrap(), 3);
    let tools: Vec<&str> = v.pointer("/data/blocked_producers").unwrap()
        .as_array().unwrap().iter()
        .map(|p| p["tool"].as_str().unwrap())
        .collect();
    assert_eq!(tools, vec!["a", "b", "c"]);
}

#[test]
fn list_on_empty_store_returns_empty() {
    let store = tempdir().unwrap();
    let out = run(&[
        "--output", "json",
        "policy", "list",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/count").unwrap(), 0);
}

#[test]
fn block_requires_reason() {
    let store = tempdir().unwrap();
    let out = run(&[
        "policy", "block-producer", "bot",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("reason"), "stderr should mention reason: {stderr}");
}

#[test]
fn unblock_unknown_producer_succeeds_idempotently() {
    let store = tempdir().unwrap();
    let out = run(&[
        "--output", "json",
        "policy", "unblock-producer", "ghost",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/was_blocked").unwrap(), false);
}
