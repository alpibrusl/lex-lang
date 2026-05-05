//! `lex branch overlay <other> [--on <branch>]` — preview merge
//! result without committing (#133).

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

fn overlay_json(store: &std::path::Path, other: &str, extra: &[&str]) -> serde_json::Value {
    let mut args: Vec<String> = vec![
        "--output".into(), "json".into(),
        "branch".into(), "overlay".into(),
        "--store".into(), store.to_str().unwrap().to_string(),
        other.into(),
    ];
    for e in extra { args.push((*e).to_string()); }
    let out = Command::new(lex_bin())
        .args(&args)
        .output()
        .expect("overlay");
    assert!(out.status.success(), "overlay: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn overlay_clean_merge_projects_added_sigs_into_dst() {
    // main has fn foo. feature adds fn bar (disjoint). Overlay
    // feature on main should auto-resolve and project bar in.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store.path(), &src);

    let s = lex_store::Store::open(store.path()).unwrap();
    s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
    s.set_current_branch("feature").unwrap();
    drop(s);
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\nfn bar(n :: Int) -> Int { n + 1 }\n").unwrap();
    publish(store.path(), &src);

    // Switch back to main so it's the dst (current).
    let s = lex_store::Store::open(store.path()).unwrap();
    s.set_current_branch(lex_store::DEFAULT_BRANCH).unwrap();
    drop(s);

    let v = overlay_json(store.path(), "feature", &[]);
    assert_eq!(v.pointer("/data/this_branch").unwrap(), lex_store::DEFAULT_BRANCH);
    assert_eq!(v.pointer("/data/other_branch").unwrap(), "feature");
    let conflicts = v.pointer("/data/conflicts").unwrap().as_array().unwrap();
    assert_eq!(conflicts.len(), 0, "disjoint adds should auto-resolve");
    let projected = v.pointer("/data/projected_head").unwrap().as_object().unwrap();
    assert_eq!(projected.len(), 2, "projection should contain both foo and bar");
}

#[test]
fn overlay_with_conflict_lists_it_but_keeps_projection_at_dst() {
    // main and feature both modify foo differently. Overlay should
    // surface a conflict and keep dst's stage in the projection
    // (the conflict is noted, not auto-merged).
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

    let s = lex_store::Store::open(store.path()).unwrap();
    s.set_current_branch(lex_store::DEFAULT_BRANCH).unwrap();
    drop(s);
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n + 2 }\n").unwrap();
    publish(store.path(), &src);

    let v = overlay_json(store.path(), "feature", &[]);
    let conflicts = v.pointer("/data/conflicts").unwrap().as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "expected one ModifyModify conflict");
    // Projection still has foo at dst's (main's) stage.
    let projected = v.pointer("/data/projected_head").unwrap().as_object().unwrap();
    assert_eq!(projected.len(), 1);
}

#[test]
fn overlay_on_explicit_dst_works() {
    // --on <branch> overrides the current-branch default.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store.path(), &src);

    let s = lex_store::Store::open(store.path()).unwrap();
    s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
    drop(s);

    // Stay on main; overlay feature explicitly on main.
    let v = overlay_json(store.path(), "feature", &["--on", lex_store::DEFAULT_BRANCH]);
    assert_eq!(v.pointer("/data/this_branch").unwrap(), lex_store::DEFAULT_BRANCH);
    assert_eq!(v.pointer("/data/other_branch").unwrap(), "feature");
}

#[test]
fn overlay_unknown_other_errors() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store.path(), &src);

    let out = Command::new(lex_bin())
        .args([
            "branch", "overlay",
            "--store", store.path().to_str().unwrap(),
            "no_such_branch",
        ])
        .output().unwrap();
    assert!(!out.status.success());
}
