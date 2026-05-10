//! Conformance tests for #283 — `lex store search reindex`.
//!
//! These exercise the CLI's wiring without making any external
//! HTTP calls: `LEX_EMBED_URL` stays unset, so `build_embedder`
//! falls back to `MockEmbedder` (deterministic, no network). The
//! test validates that the reindex command walks every active
//! stage and reports a non-zero indexed count.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

fn publish(tmp: &std::path::Path, src_name: &str, body: &str, activate: bool) {
    let src = tmp.join(src_name);
    std::fs::write(&src, body).unwrap();
    let mut args = vec!["publish".to_string(), src.to_str().unwrap().to_string(),
        "--store".to_string(), tmp.to_str().unwrap().to_string()];
    if activate { args.push("--activate".into()); }
    let out = Command::new(lex_bin()).args(&args).output().unwrap();
    assert!(out.status.success(),
        "publish failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr));
}

fn reindex(tmp: &std::path::Path) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args(["--output", "json", "store", "search", "reindex",
            "--store", tmp.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(),
        "reindex failed: stderr={}", String::from_utf8_lossy(&out.stderr));
    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    env["data"].clone()
}

#[test]
fn reindex_on_empty_store_reports_zero_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    let v = reindex(tmp.path());
    assert_eq!(v["indexed"], 0);
    assert!(v["dim"].as_u64().unwrap() > 0,
        "embedder must report a positive dim");
    assert!(v["elapsed_ms"].is_u64());
    assert!(v["store"].is_string());
}

#[test]
fn reindex_picks_up_published_active_stages() {
    let tmp = tempfile::tempdir().unwrap();

    // Publish two fns as drafts — reindex must skip drafts.
    publish(tmp.path(), "a.lex", "fn alpha(n :: Int) -> Int { n + 1 }\n", false);
    publish(tmp.path(), "b.lex", "fn beta(n :: Int) -> Int { n + 2 }\n", false);

    let pre = reindex(tmp.path());
    assert_eq!(pre["indexed"], 0,
        "drafts should not be indexed; got {}", pre["indexed"]);

    // Re-publish alpha with --activate; that activates the stage.
    publish(tmp.path(), "a.lex", "fn alpha(n :: Int) -> Int { n + 1 }\n", true);

    let post = reindex(tmp.path());
    assert_eq!(post["indexed"], 1, "exactly the activated stage indexed");
}
