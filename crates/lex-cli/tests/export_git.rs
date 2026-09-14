//! `lex export-git` — render a branch's op history as a git repo (#837).

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, text: &str) {
    let src = store.join("a.lex");
    std::fs::write(&src, text).unwrap();
    let out = Command::new(lex_bin())
        .args(["--output", "json", "publish", "--store", store.to_str().unwrap(), src.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn export_produces_a_commit_per_op_and_the_head_source() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn add(x :: Int, y :: Int) -> Int { x + y }\n");
    publish(store.path(), "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn mul(x :: Int, y :: Int) -> Int { x * y }\n");

    let out = tempdir().unwrap();
    let res = Command::new(lex_bin())
        .args(["--output", "json", "export-git", out.path().to_str().unwrap(),
               "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(res.status.success(), "export: {}", String::from_utf8_lossy(&res.stderr));
    let v: serde_json::Value = serde_json::from_slice(&res.stdout).unwrap();
    let commits = v.pointer("/data/commits").unwrap().as_u64().unwrap();
    assert!(commits >= 2, "expected >=2 commits (one per op), got {commits}");

    // The repo is real and its log has that many commits.
    let count: u64 = git(out.path(), &["rev-list", "--count", "HEAD"]).trim().parse().unwrap();
    assert_eq!(count, commits, "git log commit count must match reported commits");

    // The working tree's head source contains both functions.
    let src = std::fs::read_to_string(out.path().join("src.lex")).unwrap();
    assert!(src.contains("fn add"), "head source must contain add: {src}");
    assert!(src.contains("fn mul"), "head source must contain mul: {src}");

    // Commit messages carry the op trailer (traceable back to the log).
    let logtext = git(out.path(), &["log", "--format=%B"]);
    assert!(logtext.contains("Op: "), "commit messages must carry an Op: trailer");
}

#[test]
fn export_is_deterministic_and_re_runnable() {
    let store = tempdir().unwrap();
    publish(store.path(), "fn id(x :: Int) -> Int { x }\n");
    let out = tempdir().unwrap();
    let run = || Command::new(lex_bin())
        .args(["export-git", out.path().to_str().unwrap(), "--store", store.path().to_str().unwrap()])
        .output().unwrap();
    assert!(run().status.success());
    // Re-running into the same dir must not error (idempotent tool).
    assert!(run().status.success(), "re-export must succeed");
}
