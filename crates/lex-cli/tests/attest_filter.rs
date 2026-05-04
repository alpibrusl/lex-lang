//! `lex attest filter` — cross-stage attestation queries (#132).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, src: &std::path::Path) {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "publish failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn attest_filter_json(store: &std::path::Path, extra: &[&str]) -> serde_json::Value {
    let mut args: Vec<String> = vec![
        "--output".into(), "json".into(),
        "attest".into(), "filter".into(),
        "--store".into(), store.to_str().unwrap().to_string(),
    ];
    for e in extra { args.push((*e).to_string()); }
    let out = Command::new(lex_bin())
        .args(&args)
        .output()
        .expect("run attest filter");
    assert!(out.status.success(), "attest filter failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn filter_lists_all_attestations_when_no_filter_given() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &src);

    let v = attest_filter_json(store.path(), &[]);
    let count = v.pointer("/data/count").unwrap().as_u64().unwrap();
    assert!(count >= 1, "publish should have produced ≥1 TypeCheck attestation");
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    assert_eq!(atts.len() as u64, count);
}

#[test]
fn filter_by_kind_narrows_results() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &src);

    // Only TypeCheck attestations exist; filtering by `spec` returns 0.
    let v = attest_filter_json(store.path(), &["--kind", "spec"]);
    assert_eq!(v.pointer("/data/count").unwrap().as_u64().unwrap(), 0);

    // Filtering by `type_check` returns the publish attestation.
    let v = attest_filter_json(store.path(), &["--kind", "type_check"]);
    let count = v.pointer("/data/count").unwrap().as_u64().unwrap();
    assert!(count >= 1);
    let atts = v.pointer("/data/attestations").unwrap().as_array().unwrap();
    for a in atts {
        assert_eq!(a["kind"]["kind"], "type_check");
    }
}

#[test]
fn filter_by_result_narrows_results() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &src);

    let v = attest_filter_json(store.path(), &["--result", "passed"]);
    assert!(v.pointer("/data/count").unwrap().as_u64().unwrap() >= 1);

    let v = attest_filter_json(store.path(), &["--result", "failed"]);
    assert_eq!(v.pointer("/data/count").unwrap().as_u64().unwrap(), 0);
}

#[test]
fn filter_since_in_the_future_returns_zero() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &src);

    // Year 2999 → no attestation can possibly be ≥ this.
    let v = attest_filter_json(store.path(), &["--since", "2999-01-01"]);
    assert_eq!(v.pointer("/data/count").unwrap().as_u64().unwrap(), 0);
}

#[test]
fn filter_since_at_epoch_returns_everything() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &src);

    // 0 = epoch; every attestation has a timestamp ≥ 0.
    let v = attest_filter_json(store.path(), &["--since", "0"]);
    assert!(v.pointer("/data/count").unwrap().as_u64().unwrap() >= 1);
}

#[test]
fn filter_rejects_malformed_since() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "attest", "filter",
            "--store", store.path().to_str().unwrap(),
            "--since", "yesterday",
        ])
        .output().unwrap();
    assert!(!out.status.success(), "expected error on malformed --since");
}

#[test]
fn empty_store_returns_empty_list() {
    let store = tempdir().unwrap();
    let v = attest_filter_json(store.path(), &[]);
    assert_eq!(v.pointer("/data/count").unwrap().as_u64().unwrap(), 0);
}
