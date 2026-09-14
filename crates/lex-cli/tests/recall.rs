//! `lex recall` — predicate queries over the op log (#836 G2).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, src_text: &str) {
    let src = store.join("a.lex");
    std::fs::write(&src, src_text).unwrap();
    let out = Command::new(lex_bin())
        .args(["--output", "json", "publish", "--store", store.to_str().unwrap(), src.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
}

fn recall(store: &std::path::Path, extra: &[&str]) -> (bool, serde_json::Value) {
    let mut args: Vec<String> = vec!["--output".into(), "json".into(), "recall".into(),
        "--store".into(), store.to_str().unwrap().into()];
    for e in extra { args.push((*e).into()); }
    let out = Command::new(lex_bin()).args(&args).output().unwrap();
    let v = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (out.status.success(), v)
}

#[test]
fn recall_all_lists_every_op() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn a() -> Int { 1 }\n");
    publish(store.path(), "fn a() -> Int { 1 }\nfn b() -> Int { 2 }\n");

    let (ok, v) = recall(store.path(), &["--all"]);
    assert!(ok, "recall --all failed");
    let count = v.pointer("/data/count").unwrap().as_u64().unwrap();
    assert!(count >= 2, "expected >=2 ops, got {count}");
    assert_eq!(v.pointer("/data/ops").unwrap().as_array().unwrap().len() as u64, count);
}

#[test]
fn recall_by_intent_narrows_and_limit_caps() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn a() -> Int { 1 }\n");

    // No op carries this intent → empty.
    let (ok, v) = recall(store.path(), &["--intent", "does-not-exist"]);
    assert!(ok);
    assert_eq!(v.pointer("/data/count").unwrap().as_u64().unwrap(), 0);

    // --limit caps the result set.
    let (ok, v) = recall(store.path(), &["--all", "--limit", "1"]);
    assert!(ok);
    assert!(v.pointer("/data/count").unwrap().as_u64().unwrap() <= 1);
}

#[test]
fn recall_predicate_json_is_evaluated() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn a() -> Int { 1 }\n");
    let (ok, v) = recall(store.path(), &["--predicate", r#"{"predicate":"all"}"#]);
    assert!(ok, "predicate recall failed");
    assert!(v.pointer("/data/count").unwrap().as_u64().unwrap() >= 1);
}

#[test]
fn recall_requires_exactly_one_selector() {
    let store = tempdir().unwrap();
    // none
    let out = Command::new(lex_bin())
        .args(["recall", "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(!out.status.success(), "no selector must error");
    // two
    let out = Command::new(lex_bin())
        .args(["recall", "--store", store.path().to_str().unwrap(), "--all", "--intent", "x"])
        .output().unwrap();
    assert!(!out.status.success(), "two selectors must error");
}
