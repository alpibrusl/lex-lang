//! `lex branch peek <name> [--since-fork]` — read another branch's
//! ops without switching to it (#133).

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
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
}

fn peek_json(store: &std::path::Path, branch: &str, extra: &[&str]) -> serde_json::Value {
    let mut args: Vec<String> = vec![
        "--output".into(), "json".into(),
        "branch".into(), "peek".into(),
        "--store".into(), store.to_str().unwrap().to_string(),
        branch.into(),
    ];
    for e in extra { args.push((*e).to_string()); }
    let out = Command::new(lex_bin())
        .args(&args)
        .output()
        .expect("peek");
    assert!(out.status.success(), "peek: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn peek_full_ancestry_lists_every_op() {
    // Without --since-fork, peek returns every op reachable from
    // the branch head. After two publishes on main, we expect ≥2 ops.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store.path(), &src);
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n + 1 }\n").unwrap();
    publish(store.path(), &src);

    let v = peek_json(store.path(), lex_store::DEFAULT_BRANCH, &[]);
    let ops = v.pointer("/data/ops").unwrap().as_array().unwrap();
    assert!(ops.len() >= 2, "full ancestry should include ≥2 ops, got {}", ops.len());
    assert_eq!(v.pointer("/data/since_fork").unwrap(), false);
}

#[test]
fn peek_since_fork_excludes_shared_ancestors() {
    // Set up: main has fn foo. feature branches off, modifies foo.
    // peek feature --since-fork should return ONLY feature's
    // post-fork op, not the original publish (which is shared).
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store.path(), &src);

    let s = lex_store::Store::open(store.path()).unwrap();
    s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
    s.set_current_branch("feature").unwrap();
    drop(s);

    std::fs::write(&src, "fn foo(n :: Int) -> Int { n + 1 }\n").unwrap();
    publish(store.path(), &src);

    let v = peek_json(store.path(), "feature", &["--since-fork"]);
    let ops = v.pointer("/data/ops").unwrap().as_array().unwrap();
    assert_eq!(ops.len(), 1, "feature should have one post-fork op, got: {ops:?}");
    assert_eq!(v.pointer("/data/since_fork").unwrap(), true);
    let fork_point = v.pointer("/data/fork_point").unwrap();
    assert!(fork_point.is_string(), "fork_point should be set; got {fork_point:?}");
}

#[test]
fn peek_since_fork_with_explicit_vs_compares_against_named_branch() {
    // Same shape but using --vs <other> instead of the recorded parent.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store.path(), &src);

    let s = lex_store::Store::open(store.path()).unwrap();
    s.create_branch("a", lex_store::DEFAULT_BRANCH).unwrap();
    s.create_branch("b", lex_store::DEFAULT_BRANCH).unwrap();
    s.set_current_branch("a").unwrap();
    drop(s);
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n + 1 }\n").unwrap();
    publish(store.path(), &src);

    let v = peek_json(store.path(), "a", &["--since-fork", "--vs", "b"]);
    let ops = v.pointer("/data/ops").unwrap().as_array().unwrap();
    assert_eq!(ops.len(), 1, "a's post-fork-from-b op set should have one entry");
}

#[test]
fn peek_unknown_branch_errors() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "branch", "peek",
            "--store", store.path().to_str().unwrap(),
            "no_such_branch",
        ])
        .output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn peek_empty_branch_returns_empty_ops_list() {
    // A fresh store with no publishes has an empty default branch
    // (no head_op). peek should not error — just return ops: [].
    let store = tempdir().unwrap();
    let v = peek_json(store.path(), lex_store::DEFAULT_BRANCH, &[]);
    let ops = v.pointer("/data/ops").unwrap().as_array().unwrap();
    assert_eq!(ops.len(), 0);
}
