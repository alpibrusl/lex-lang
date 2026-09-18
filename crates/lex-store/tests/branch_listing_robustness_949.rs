//! #949 prod finding: `branches/` also holds each branch's persisted head
//! snapshot (`<branch>.head_snapshot.json`). `list_branches` used to report
//! its stem as a phantom branch, and deriving issue state then failed on
//! `get_branch("main.head_snapshot")` ("missing field `name`") — which took
//! every board endpoint down. Two guarantees:
//!
//! 1. snapshot files never surface as branches;
//! 2. an unreadable branch record degrades to "no provenance from that
//!    branch" instead of failing the whole derivation.

use std::collections::BTreeSet;
use std::fs;

use lex_store::issues::{all_issue_status, issues_in_progress, IssueState};
use lex_store::{Store, DEFAULT_BRANCH};
use lex_vcs::{Acceptance, Issue, IssueLog};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn branches_dir(store: &Store) -> std::path::PathBuf {
    // Created lazily by the store on first branch write.
    let dir = store.root().join("branches");
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn head_snapshot_files_are_not_branches() {
    let (store, _tmp) = fresh();
    let dir = branches_dir(&store);
    // What `save_head_snapshot` writes for main once the head grows.
    fs::write(
        dir.join(format!("{DEFAULT_BRANCH}.head_snapshot.json")),
        r#"{"head_op":"deadbeef","map":{"gcd":"stage-1"}}"#,
    )
    .unwrap();
    // A real sibling branch still lists.
    store.create_branch("feature", DEFAULT_BRANCH).unwrap();
    fs::write(dir.join("feature.head_snapshot.json"), r#"{"head_op":"x","map":{}}"#).unwrap();

    let mut listed = store.list_branches().unwrap();
    listed.sort();
    assert_eq!(listed, vec!["feature".to_string(), DEFAULT_BRANCH.to_string()]);
}

#[test]
fn issue_state_survives_stray_and_unreadable_branch_files() {
    let (store, _tmp) = fresh();
    let dir = branches_dir(&store);
    fs::write(
        dir.join(format!("{DEFAULT_BRANCH}.head_snapshot.json")),
        r#"{"head_op":"deadbeef","map":{}}"#,
    )
    .unwrap();
    // A branch record that doesn't deserialize (the exact prod failure mode
    // if any other file ever lands here).
    fs::write(dir.join("junk.json"), r#"{"not":"a branch"}"#).unwrap();

    let log = IssueLog::open(store.root()).unwrap();
    let issue = Issue::with_timestamp(
        "board must load",
        "",
        Acceptance::FreeForm {},
        None,
        BTreeSet::new(),
        Some("nt".into()),
        1,
    );
    log.put(&issue).unwrap();
    let id = issue.issue_id.clone();

    assert_eq!(issues_in_progress(&store).unwrap(), BTreeSet::new());
    let status = all_issue_status(&store).unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].issue.issue_id, id);
    assert_eq!(status[0].state, IssueState::Open);
    assert!(!status[0].has_work);
}
