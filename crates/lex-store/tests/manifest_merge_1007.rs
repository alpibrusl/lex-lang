//! #1007 PR 7 §1: merging disagreeing files manifests.
//!
//! `Store::manifest_merge` runs a git-style 3-way diff over two branches'
//! manifests: a path edited on only one side auto-resolves; a path edited
//! *differently* on both sides is a [`lex_vcs::FileConflict`] that needs an
//! explicit [`lex_vcs::FileResolution`]. `Store::build_merged_manifest`
//! turns the diff plus resolutions into the manifest a merge's `SetFiles`
//! op should carry, and `Store::apply_merge_op_gated_with_manifest` lands
//! both ops atomically (rolling all the way back if the follow-up
//! `SetFiles` fails).

use std::collections::BTreeMap;

use lex_store::files::{Entry, Manifest, ManifestAt};
use lex_store::{ManifestMergeOutcome, Operation, OperationKind, StageTransition, Store};
use lex_store::DEFAULT_BRANCH as MAIN;
use lex_vcs::{FileConflict, FileResolution};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn head(s: &Store, branch: &str) -> Option<String> {
    s.get_branch(branch).unwrap().and_then(|b| b.head_op)
}

/// Store `fs` as blobs plus their manifest; return the manifest id.
fn manifest_of(s: &Store, fs: &[(&str, &[u8])]) -> String {
    let mut m = Manifest::new();
    for (path, bytes) in fs {
        let blob = s.put_blob_bytes(bytes).unwrap();
        m.entries.insert(
            path.to_string(),
            Entry { blob, mode: "100644".into(), size: bytes.len() as u64 },
        );
    }
    s.put_manifest(&m).unwrap()
}

/// A trivial semantic op so branches have something besides `SetFiles` in
/// their history (manifest_merge only cares about files, but a store with
/// zero ops is a degenerate case we don't want to accidentally rely on).
fn add_fn(s: &Store, branch: &str, sig: &str) -> String {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: format!("stage-{sig}"),
            effects: Default::default(),
            budget_cost: None,
            in_file: None,
        },
        head(s, branch),
    );
    let t = StageTransition::Create { sig_id: sig.into(), stage_id: format!("stage-{sig}") };
    s.apply_operation(branch, op, t).unwrap()
}

fn set_files(s: &Store, branch: &str, manifest: &str) -> String {
    s.apply_set_files(branch, manifest, None).unwrap()
}

#[test]
fn identical_manifests_need_no_setfiles() {
    let (s, _tmp) = fresh();
    add_fn(&s, MAIN, "fn::base");
    let m = manifest_of(&s, &[("README.md", b"hello")]);
    set_files(&s, MAIN, &m);
    s.create_branch("feature", MAIN).unwrap();
    // Both branches carry the exact same manifest (feature never touched
    // files) — `manifest_at` agrees, so no SetFiles is needed at all.
    let lca = Some(head(&s, MAIN).unwrap());
    let outcome = s
        .manifest_merge(lca.as_deref(), head(&s, MAIN).as_deref(), head(&s, "feature").as_deref())
        .unwrap();
    assert_eq!(outcome, ManifestMergeOutcome::NoChange);
}

#[test]
fn both_absent_need_no_setfiles() {
    let (s, _tmp) = fresh();
    let base = add_fn(&s, MAIN, "fn::base");
    s.create_branch("feature", MAIN).unwrap();
    add_fn(&s, "feature", "fn::feat");
    add_fn(&s, MAIN, "fn::main2");
    // Neither branch ever set files: both `Absent`.
    let outcome = s
        .manifest_merge(Some(&base), head(&s, MAIN).as_deref(), head(&s, "feature").as_deref())
        .unwrap();
    assert_eq!(outcome, ManifestMergeOutcome::NoChange);
}

#[test]
fn disjoint_adds_auto_resolve_into_a_union_manifest() {
    // No semantic (`AddFunction`) ops here at all — `apply_merge_op_gated`'s
    // type-check gate loads the real AST for every sig in the post-merge
    // head, and this test only cares about the files dimension, so an
    // empty sig->stage map (a files-only history) keeps the gate trivially
    // satisfied while still exercising the real gated landing path.
    let (s, _tmp) = fresh();
    let base_manifest = manifest_of(&s, &[("README.md", b"base")]);
    let lca = set_files(&s, MAIN, &base_manifest);
    s.create_branch("feature", MAIN).unwrap();

    // dst (main) adds C.txt.
    let dst_manifest = manifest_of(&s, &[("README.md", b"base"), ("C.txt", b"c")]);
    let ours_head = set_files(&s, MAIN, &dst_manifest);

    // src (feature) adds B.txt.
    let src_manifest = manifest_of(&s, &[("README.md", b"base"), ("B.txt", b"b")]);
    let theirs_head = set_files(&s, "feature", &src_manifest);

    let outcome = s.manifest_merge(Some(&lca), Some(&ours_head), Some(&theirs_head)).unwrap();
    let (auto_entries, conflicts) = match outcome {
        ManifestMergeOutcome::Needed { auto_entries, conflicts } => (auto_entries, conflicts),
        other => panic!("expected Needed, got {other:?}"),
    };
    assert!(conflicts.is_empty(), "disjoint adds must not conflict: {conflicts:?}");
    assert_eq!(auto_entries.len(), 3, "README + B.txt + C.txt");
    assert!(auto_entries.contains_key("README.md"));
    assert!(auto_entries.contains_key("B.txt"));
    assert!(auto_entries.contains_key("C.txt"));

    // Build + land the merged manifest: no conflicts, so an empty
    // resolutions map is enough.
    let merged_id = s
        .build_merged_manifest(auto_entries, &conflicts, &BTreeMap::new(), "merge-op")
        .unwrap();
    let merge_op = Operation::new(
        OperationKind::Merge { resolved: 0 },
        vec![ours_head.clone(), theirs_head.clone()],
    );
    let transition = StageTransition::Merge { entries: Default::default() };
    let new_head = s
        .apply_merge_op_gated_with_manifest(MAIN, merge_op, transition, Some(&merged_id), None)
        .unwrap();

    match s.manifest_at(&new_head).unwrap() {
        ManifestAt::Set { manifest } => {
            let m = s.get_manifest(&manifest).unwrap();
            assert_eq!(m.entries.len(), 3);
            assert!(m.entries.contains_key("B.txt"));
            assert!(m.entries.contains_key("C.txt"));
        }
        other => panic!("expected a resolved manifest, got {other:?}"),
    }
}

#[test]
fn same_path_different_edit_is_a_file_conflict() {
    let (s, _tmp) = fresh();
    let base_manifest = manifest_of(&s, &[("README.md", b"base")]);
    let lca = set_files(&s, MAIN, &base_manifest);
    s.create_branch("feature", MAIN).unwrap();

    let dst_manifest = manifest_of(&s, &[("README.md", b"left")]);
    let ours_head = set_files(&s, MAIN, &dst_manifest);
    let src_manifest = manifest_of(&s, &[("README.md", b"right")]);
    let theirs_head = set_files(&s, "feature", &src_manifest);

    let outcome = s.manifest_merge(Some(&lca), Some(&ours_head), Some(&theirs_head)).unwrap();
    let (auto_entries, conflicts) = match outcome {
        ManifestMergeOutcome::Needed { auto_entries, conflicts } => (auto_entries, conflicts),
        other => panic!("expected Needed, got {other:?}"),
    };
    assert!(auto_entries.is_empty(), "the only path is in conflict");
    assert_eq!(conflicts.len(), 1);
    let c: &FileConflict = &conflicts[0];
    assert_eq!(c.path, "README.md");
    assert!(c.base.is_some());
    assert!(c.ours.is_some());
    assert!(c.theirs.is_some());
    assert_ne!(c.ours, c.theirs);

    // Unresolved: build_merged_manifest must refuse (fail loud rather
    // than silently drop the path).
    let err = s
        .build_merged_manifest(auto_entries.clone(), &conflicts, &BTreeMap::new(), "merge-op")
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::AmbiguousManifest { .. }));

    // Resolve take_ours: the merged manifest must carry `left`'s blob.
    let mut resolutions = BTreeMap::new();
    resolutions.insert("README.md".to_string(), FileResolution::TakeOurs);
    let merged_id = s
        .build_merged_manifest(auto_entries, &conflicts, &resolutions, "merge-op")
        .unwrap();
    let merged = s.get_manifest(&merged_id).unwrap();
    assert_eq!(merged.entries.len(), 1);
    assert_eq!(merged.entries["README.md"].blob, c.ours.as_ref().unwrap().blob);
}

#[test]
fn merge_commit_refuses_ambiguous_manifest_without_setfiles() {
    // If the caller lands the merge op WITHOUT the required SetFiles
    // (e.g. an older client, or a bug), `manifest_at` on the bare merge
    // op reports `Ambiguous` — the always-valid-HEAD gate for a plain
    // head advance already covers this (`check_head_files` /
    // `blobs_sync_1007.rs::head_advance_refuses_an_ambiguous_manifest`);
    // this test documents that `apply_merge_op_gated_with_manifest`
    // with `manifest: None` behaves exactly like the sig-only
    // `apply_merge_op_gated` (no attempt to guess a manifest).
    let (s, _tmp) = fresh();
    let base_manifest = manifest_of(&s, &[("README.md", b"base")]);
    let lca = set_files(&s, MAIN, &base_manifest);
    s.create_branch("feature", MAIN).unwrap();
    let dst_manifest = manifest_of(&s, &[("README.md", b"left")]);
    let ours_head = set_files(&s, MAIN, &dst_manifest);
    let src_manifest = manifest_of(&s, &[("README.md", b"right")]);
    let theirs_head = set_files(&s, "feature", &src_manifest);
    let _ = lca;

    let merge_op = Operation::new(
        OperationKind::Merge { resolved: 0 },
        vec![ours_head.clone(), theirs_head.clone()],
    );
    let transition = StageTransition::Merge { entries: Default::default() };
    let new_head = s
        .apply_merge_op_gated_with_manifest(MAIN, merge_op, transition, None, None)
        .unwrap();
    assert_eq!(s.manifest_at(&new_head).unwrap(), ManifestAt::Ambiguous);
}
