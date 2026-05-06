//! `lex merge {start|status|resolve|commit}` — CLI mirror of the
//! /v1/merge/* HTTP API (#134). Same engine, persisted to disk so
//! each CLI invocation is its own process.

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

/// Set up a ModifyModify conflict on `foo` between `feature` and
/// the default branch, mirroring `with_modify_modify_session` in
/// the API tests.
fn modify_modify_setup(store: &std::path::Path) {
    let src = store.join("a.lex");
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n }\n").unwrap();
    publish(store, &src);

    let s = lex_store::Store::open(store).unwrap();
    s.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
    s.set_current_branch("feature").unwrap();
    drop(s);
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n + 1 }\n").unwrap();
    publish(store, &src);

    let s = lex_store::Store::open(store).unwrap();
    s.set_current_branch(lex_store::DEFAULT_BRANCH).unwrap();
    drop(s);
    std::fs::write(&src, "fn foo(n :: Int) -> Int { n + 2 }\n").unwrap();
    publish(store, &src);
}

fn merge_start(store: &std::path::Path, src: &str, dst: &str) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "start",
            "--store", store.to_str().unwrap(),
            "--src", src,
            "--dst", dst,
        ])
        .output().unwrap();
    assert!(out.status.success(), "merge start failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn merge_start_persists_session_to_disk() {
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());

    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap();
    let conflicts = v.pointer("/data/conflicts").unwrap().as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "expected one ModifyModify conflict");

    // The session file lives under <store>/merges/<merge_id>.json.
    let session_path = store.path().join("merges").join(format!("{merge_id}.json"));
    assert!(session_path.exists(), "session should be persisted at {session_path:?}");
}

#[test]
fn merge_status_shows_remaining_conflicts() {
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());
    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "status",
            "--store", store.path().to_str().unwrap(),
            merge_id,
        ])
        .output().unwrap();
    assert!(out.status.success(), "status failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v.pointer("/data/src_branch").unwrap(), "feature");
    assert_eq!(v.pointer("/data/dst_branch").unwrap(), lex_store::DEFAULT_BRANCH);
    let remaining = v.pointer("/data/remaining_conflicts").unwrap().as_array().unwrap();
    assert_eq!(remaining.len(), 1);
}

#[test]
fn merge_full_cycle_resolve_then_commit() {
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());
    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap().to_string();
    let conflict_id = v.pointer("/data/conflicts/0/conflict_id").unwrap().as_str().unwrap().to_string();

    // Write a resolutions file.
    let resolutions_path = store.path().join("res.json");
    let body = serde_json::json!([
        {"conflict_id": conflict_id, "resolution": {"kind": "take_theirs"}}
    ]);
    std::fs::write(&resolutions_path, serde_json::to_vec(&body).unwrap()).unwrap();

    // Resolve.
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "resolve",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
            "--file", resolutions_path.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "resolve failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let verdicts = v.pointer("/data/verdicts").unwrap().as_array().unwrap();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0]["accepted"], true);

    // Commit.
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "commit",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
        ])
        .output().unwrap();
    assert!(out.status.success(), "commit failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!v.pointer("/data/new_head_op").unwrap().as_str().unwrap().is_empty());
    assert_eq!(v.pointer("/data/dst_branch").unwrap(), lex_store::DEFAULT_BRANCH);

    // Session file should be gone after commit.
    let session_path = store.path().join("merges").join(format!("{merge_id}.json"));
    assert!(!session_path.exists(), "session file should be deleted after commit");
}

#[test]
fn merge_commit_with_unresolved_conflicts_errors() {
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());
    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap().to_string();

    let out = Command::new(lex_bin())
        .args([
            "merge", "commit",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
        ])
        .output().unwrap();
    assert!(!out.status.success(), "commit should fail when conflicts remain");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("conflicts remaining"), "stderr should mention conflicts: {stderr}");
}

#[test]
fn merge_full_cycle_with_custom_resolution_lands_agent_stage() {
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());
    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap().to_string();
    let conflict = v.pointer("/data/conflicts/0").unwrap();
    let conflict_id = conflict["conflict_id"].as_str().unwrap().to_string();
    let ours = conflict["ours"].as_str().unwrap().to_string();
    let theirs = conflict["theirs"].as_str().unwrap().to_string();

    let resolutions_path = store.path().join("res.json");
    // Same shape as the HTTP /resolve body. Operation's `kind` is
    // serde-flattened so the OperationKind tag lives at the top.
    let body = serde_json::json!([{
        "conflict_id": conflict_id,
        "resolution": {
            "kind": "custom",
            "op": {
                "op": "modify_body",
                "sig_id": conflict_id,
                "from_stage_id": ours,
                "to_stage_id":   "stage-agent-resolved-cli-001",
                "parents": [ours, theirs],
            }
        }
    }]);
    std::fs::write(&resolutions_path, serde_json::to_vec(&body).unwrap()).unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "resolve",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
            "--file", resolutions_path.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "resolve: {}", String::from_utf8_lossy(&out.stderr));

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "commit",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
        ])
        .output().unwrap();
    assert!(out.status.success(), "commit: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(!v.pointer("/data/new_head_op").unwrap().as_str().unwrap().is_empty());
}

#[test]
fn merge_status_unknown_id_errors() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "merge", "status",
            "--store", store.path().to_str().unwrap(),
            "no_such_merge",
        ])
        .output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn merge_defer_resolves_one_conflict_and_blocks_commit() {
    // `lex merge defer <merge_id> <conflict_id>` is the per-
    // conflict shortcut that #181 calls out: same effect as
    // putting `Resolution::Defer` in a resolutions file but no
    // tempfile dance for the common "park this for review" path.
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());
    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap().to_string();
    let conflict_id = v.pointer("/data/conflicts/0/conflict_id").unwrap().as_str().unwrap().to_string();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "defer",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
            &conflict_id,
        ])
        .output().unwrap();
    assert!(out.status.success(), "defer: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let verdict = v.pointer("/data/verdicts/0").unwrap();
    assert_eq!(verdict["accepted"], true, "defer should be accepted: {v}");
    assert_eq!(verdict["conflict_id"], conflict_id);

    // Defer leaves the conflict in remaining_conflicts so commit
    // refuses — that's the whole point: punt-to-human, not auto-
    // resolve.
    let out = Command::new(lex_bin())
        .args([
            "merge", "commit",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
        ])
        .output().unwrap();
    assert!(!out.status.success(), "commit must refuse a deferred conflict");
}

#[test]
fn merge_defer_unknown_conflict_returns_rejection_verdict() {
    let store = tempdir().unwrap();
    modify_modify_setup(store.path());
    let v = merge_start(store.path(), "feature", lex_store::DEFAULT_BRANCH);
    let merge_id = v.pointer("/data/merge_id").unwrap().as_str().unwrap().to_string();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "merge", "defer",
            "--store", store.path().to_str().unwrap(),
            &merge_id,
            "no_such_conflict",
        ])
        .output().unwrap();
    // Per-conflict rejection lives in the verdict JSON, not the
    // process exit — matches `lex merge resolve` shape.
    assert!(out.status.success(),
        "defer should print verdict, not exit-fail: {}",
        String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let verdict = v.pointer("/data/verdicts/0").unwrap();
    assert_eq!(verdict["accepted"], false, "rejection expected: {v}");
}

#[test]
fn merge_defer_requires_both_args() {
    let store = tempdir().unwrap();
    let out = Command::new(lex_bin())
        .args([
            "merge", "defer",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage"), "stderr should have usage: {stderr}");
}
