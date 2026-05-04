//! `lex spec check --store DIR` — Spec attestation producer (#132).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

const CLAMP_GOOD: &str = "fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {\n  match x < lo {\n    true => lo,\n    false => match x > hi {\n      true => hi,\n      false => x,\n    },\n  }\n}\n";

const CLAMP_BAD: &str = "fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {\n  match x < lo {\n    true => x,\n    false => match x > hi {\n      true => x,\n      false => x,\n    },\n  }\n}\n";

const CLAMP_SPEC: &str = "spec clamp {\n  forall x :: Int, lo :: Int, hi :: Int where lo <= hi:\n    let r := clamp(x, lo, hi)\n    (r >= lo) and (r <= hi)\n}\n";

fn write_files(store: &std::path::Path, lex_src: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let lex = store.join("clamp.lex");
    let spec = store.join("clamp.spec");
    std::fs::write(&lex, lex_src).unwrap();
    std::fs::write(&spec, CLAMP_SPEC).unwrap();
    (lex, spec)
}

fn publish(store: &std::path::Path, lex: &std::path::Path) {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.to_str().unwrap(),
            lex.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "publish failed: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn passing_spec_writes_passed_attestation() {
    let store = tempdir().unwrap();
    let (lex, spec) = write_files(store.path(), CLAMP_GOOD);
    publish(store.path(), &lex);

    let out = Command::new(lex_bin())
        .args([
            "spec", "check",
            spec.to_str().unwrap(),
            "--source", lex.to_str().unwrap(),
            "--store", store.path().to_str().unwrap(),
            "--trials", "200",
        ])
        .output()
        .expect("run spec check");
    assert!(out.status.success(), "spec check failed: {}", String::from_utf8_lossy(&out.stderr));

    // The clamp fn should now have a Spec attestation recorded.
    let stage_out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "blame",
            "--store", store.path().to_str().unwrap(),
            "--with-evidence",
            lex.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(stage_out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&stage_out.stdout).unwrap();
    let blame = v.pointer("/data/blame").unwrap().as_array().unwrap();
    let entry = blame.iter().find(|e| e["name"] == "clamp").expect("clamp entry");
    let history = entry["history"].as_array().unwrap();
    let atts = history[0]["attestations"].as_array().unwrap();

    let spec_att = atts.iter().find(|a| a["kind"]["kind"] == "spec")
        .expect("expected a Spec attestation alongside the TypeCheck one");
    assert_eq!(spec_att["result"]["result"], "passed");
    assert_eq!(spec_att["produced_by"]["tool"], "lex spec check");
    // spec_id is a content hash from the spec_checker, not the spec
    // name — non-empty and stable across runs.
    let spec_id = spec_att["kind"]["spec_id"].as_str().expect("spec_id string");
    assert!(!spec_id.is_empty(), "spec_id should be non-empty");
    assert_eq!(spec_att["kind"]["method"], "random");
    assert_eq!(spec_att["kind"]["trials"], 200);
}

#[test]
fn counterexample_writes_failed_attestation() {
    let store = tempdir().unwrap();
    let (lex, spec) = write_files(store.path(), CLAMP_BAD);
    publish(store.path(), &lex);

    // CLAMP_BAD violates the spec; exit code 5 expected.
    let out = Command::new(lex_bin())
        .args([
            "spec", "check",
            spec.to_str().unwrap(),
            "--source", lex.to_str().unwrap(),
            "--store", store.path().to_str().unwrap(),
            "--trials", "200",
        ])
        .output()
        .expect("run spec check");
    assert_eq!(out.status.code(), Some(5), "expected exit 5 for counterexample");

    // Even on failure, the attestation must be persisted — failures
    // are evidence too (#132 trust model).
    let stage_out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "blame",
            "--store", store.path().to_str().unwrap(),
            "--with-evidence",
            lex.to_str().unwrap(),
        ])
        .output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&stage_out.stdout).unwrap();
    let blame = v.pointer("/data/blame").unwrap().as_array().unwrap();
    let entry = blame.iter().find(|e| e["name"] == "clamp").unwrap();
    let history = entry["history"].as_array().unwrap();
    let atts = history[0]["attestations"].as_array().unwrap();
    let spec_att = atts.iter().find(|a| a["kind"]["kind"] == "spec")
        .expect("expected a Failed Spec attestation");
    assert_eq!(spec_att["result"]["result"], "failed");
    let detail = spec_att["result"]["detail"].as_str().unwrap_or("");
    assert!(detail.starts_with("counterexample"), "detail should describe the counterexample, got: {detail}");
}

#[test]
fn no_store_means_no_attestation() {
    // Without --store, the spec checker still runs and reports the
    // verdict, but writes nothing. Backwards-compat for the prior
    // CLI surface.
    let store = tempdir().unwrap();
    let (lex, spec) = write_files(store.path(), CLAMP_GOOD);
    let out = Command::new(lex_bin())
        .args([
            "spec", "check",
            spec.to_str().unwrap(),
            "--source", lex.to_str().unwrap(),
            "--trials", "100",
        ])
        .output().unwrap();
    assert!(out.status.success());
    // No attestation directory should have been created.
    assert!(!store.path().join("attestations").exists());
}
