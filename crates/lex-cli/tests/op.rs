//! `lex op show` and `lex op log`.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, src: &std::path::Path) {
    let out = Command::new(lex_bin())
        .args([
            "--output","json","publish","--store",store.to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn op_log_walks_branch_head_back() {
    let store = tempdir().unwrap();
    let a = store.path().join("a.lex");
    std::fs::write(&a, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &a);
    std::fs::write(&a, "fn fac(n :: Int) -> Int { 2 }\n").unwrap();
    publish(store.path(), &a);

    let out = Command::new(lex_bin())
        .args([
            "--output","json","op","log",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = v.pointer("/data/log").or_else(|| v.get("log")).unwrap();
    assert!(entries.as_array().unwrap().len() >= 2);
}

#[test]
fn op_show_returns_record() {
    let store = tempdir().unwrap();
    let a = store.path().join("a.lex");
    std::fs::write(&a, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &a);

    let log_out = Command::new(lex_bin())
        .args(["--output","json","op","log","--store",store.path().to_str().unwrap()])
        .output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&log_out.stdout).unwrap();
    let entries = v.pointer("/data/log").or_else(|| v.get("log")).unwrap();
    let first = &entries.as_array().unwrap()[0];
    let op_id = first["op_id"].as_str().unwrap().to_string();

    let show_out = Command::new(lex_bin())
        .args(["--output","json","op","show","--store",store.path().to_str().unwrap(), &op_id])
        .output().unwrap();
    assert!(show_out.status.success(), "stderr: {}", String::from_utf8_lossy(&show_out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&show_out.stdout).unwrap();
    let rec = v.pointer("/data/op").or_else(|| v.get("op")).unwrap();
    assert_eq!(rec["op_id"].as_str().unwrap(), op_id);
}
