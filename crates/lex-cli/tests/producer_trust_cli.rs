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
            "--output",
            "json",
            "producer-trust",
            "recompute",
            "--tool",
            "test-tool",
            "--store",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(env["data"]["ok"], false);
    assert!(env["data"]["reason"]
        .as_str()
        .unwrap()
        .contains("no attestations"));
}

#[test]
fn recompute_requires_tool_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "producer-trust",
            "recompute",
            "--store",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--tool"),
        "expected --tool requirement, got: {stderr}"
    );
}

#[test]
fn keyring_exports_only_producers_above_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let store = lex_store::Store::open(tmp.path()).unwrap();
    let producer = |t: &str| lex_vcs::ProducerDescriptor {
        tool: t.into(),
        version: "test".into(),
        model: None,
    };
    let push = |stage: &str, tool: &str, res| {
        let att = lex_vcs::Attestation::new(
            stage.to_string(),
            None,
            None,
            lex_vcs::AttestationKind::TypeCheck,
            res,
            producer(tool),
            None,
        );
        store.attestation_log().unwrap().put(&att).unwrap();
    };
    // "key-aaa": 10/10 → 1000 (trusted); "key-bbb": 5/10 → 500 (below threshold).
    for i in 0..10 {
        push(
            &format!("a{i}"),
            "key-aaa",
            lex_vcs::AttestationResult::Passed,
        );
    }
    for i in 0..5 {
        push(
            &format!("b{i}"),
            "key-bbb",
            lex_vcs::AttestationResult::Passed,
        );
    }
    for i in 5..10 {
        push(
            &format!("b{i}"),
            "key-bbb",
            lex_vcs::AttestationResult::Failed { detail: "x".into() },
        );
    }
    store
        .recompute_producer_trust("key-aaa", 1000, "admin")
        .unwrap()
        .unwrap();
    store
        .recompute_producer_trust("key-bbb", 1000, "admin")
        .unwrap()
        .unwrap();

    // keyring at min-trust 700 → only key-aaa, in the capsule keyring shape.
    let out = Command::new(lex_bin())
        .args([
            "producer-trust",
            "keyring",
            "--store",
            tmp.path().to_str().unwrap(),
            "--min-trust",
            "700",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let keyring: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let trusted: Vec<&str> = keyring["trusted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        trusted,
        vec!["key-aaa"],
        "only producers with score >= 700 are exported as trusted keys"
    );
}

#[test]
fn unknown_subcommand_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "producer-trust",
            "delete-all",
            "--store",
            tmp.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown"),
        "expected unknown-subcommand message, got: {stderr}"
    );
}
