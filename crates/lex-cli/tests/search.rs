//! End-to-end tests for `lex store search` and `lex audit --query`
//! (#224). The CLI uses the deterministic MockEmbedder so these
//! tests don't need a network embedding service.

use std::process::{Command, Stdio};
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn publish_one(store: &std::path::Path, name: &str, src: &str) {
    let path = store.join(format!("{name}.lex"));
    std::fs::write(&path, src).unwrap();
    let (code, _, stderr) = run(&[
        "--output", "json", "publish",
        "--store", store.to_str().unwrap(),
        "--activate",
        path.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "publish {name} failed: {stderr}");
}

#[test]
fn store_search_returns_ranked_hits() {
    let store = tempdir().unwrap();
    publish_one(store.path(),
        "csv",
        "fn parse_csv(text :: String) -> List[String] { [] }\n");
    publish_one(store.path(),
        "http",
        "fn send_post(url :: String, body :: String) -> String { url }\n");

    let (code, stdout, stderr) = run(&[
        "--output", "json", "store", "search",
        "--store", store.path().to_str().unwrap(),
        "parse csv",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hits = v.pointer("/data/hits").or_else(|| v.get("hits"))
        .expect("hits field").as_array().unwrap().clone();
    assert!(!hits.is_empty(), "expected at least one hit");
    // Top hit should be parse_csv since the query shares tokens.
    assert_eq!(hits[0]["name"].as_str(), Some("parse_csv"),
        "expected parse_csv on top; got: {hits:?}");
}

#[test]
fn store_search_text_output_lists_hits() {
    let store = tempdir().unwrap();
    publish_one(store.path(),
        "alpha",
        "fn alpha() -> Int { 1 }\n");

    let (code, stdout, _) = run(&[
        "store", "search",
        "--store", store.path().to_str().unwrap(),
        "alpha",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("alpha"),
        "text mode should mention the matched fn; got: {stdout}");
    assert!(stdout.contains("hit"),
        "text mode should announce hit count; got: {stdout}");
}

#[test]
fn store_search_limit_caps_result_count() {
    let store = tempdir().unwrap();
    for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
        publish_one(store.path(),
            name,
            &format!("fn fn_{name}(x :: Int) -> Int {{ x + {i} }}\n"));
    }

    let (code, stdout, _) = run(&[
        "--output", "json", "store", "search",
        "--store", store.path().to_str().unwrap(),
        "--limit", "2",
        "fn",
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hits = v.pointer("/data/hits").or_else(|| v.get("hits"))
        .unwrap().as_array().unwrap();
    assert_eq!(hits.len(), 2, "--limit 2 should cap the result count");
}

#[test]
fn store_search_empty_store_returns_zero_hits() {
    let store = tempdir().unwrap();
    // Force the store dir to exist before running search.
    std::fs::create_dir_all(store.path().join("stages")).unwrap();
    let (code, stdout, _) = run(&[
        "--output", "json", "store", "search",
        "--store", store.path().to_str().unwrap(),
        "anything",
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hits = v.pointer("/data/hits").or_else(|| v.get("hits"))
        .unwrap().as_array().unwrap();
    assert!(hits.is_empty());
}

#[test]
fn store_search_missing_query_errors() {
    let store = tempdir().unwrap();
    let (code, _, stderr) = run(&[
        "store", "search",
        "--store", store.path().to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(stderr.to_lowercase().contains("usage")
        || stderr.contains("query"),
        "stderr should mention required query; got: {stderr}");
}

#[test]
fn audit_query_runs_against_store() {
    let store = tempdir().unwrap();
    publish_one(store.path(),
        "alpha",
        "fn parse_csv(text :: String) -> List[String] { [] }\n");
    publish_one(store.path(),
        "beta",
        "fn send_post(url :: String, body :: String) -> String { url }\n");

    let (code, stdout, _) = run(&[
        "--output", "json", "audit",
        "--store", store.path().to_str().unwrap(),
        "--query", "parse csv",
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hits = v.pointer("/data/hits").or_else(|| v.get("hits"))
        .expect("hits").as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"].as_str(), Some("parse_csv"));
}

#[test]
fn audit_query_with_effect_filter_prunes_results() {
    let store = tempdir().unwrap();
    // Only beta declares an effect, so --effect io should drop alpha.
    publish_one(store.path(),
        "alpha",
        "fn pure_thing() -> Int { 1 }\n");
    publish_one(store.path(),
        "beta",
        "fn effectful_thing() -> [io] Int { 2 }\n");

    let (code, stdout, _) = run(&[
        "--output", "json", "audit",
        "--store", store.path().to_str().unwrap(),
        "--query", "thing",
        "--effect", "io",
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let hits = v.pointer("/data/hits").or_else(|| v.get("hits"))
        .unwrap().as_array().unwrap();
    assert_eq!(hits.len(), 1, "--effect filter should leave only effectful_thing");
    assert_eq!(hits[0]["name"].as_str(), Some("effectful_thing"));
}
