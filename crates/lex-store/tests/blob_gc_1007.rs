//! #1007 §3 / PR 7: blob GC.
//!
//! Mark-and-sweep over the blob space: a blob is live if it's in the
//! manifest closure of a `SetFiles` op retained by (the same rules as)
//! `Store::plan_gc`, or bound under `blobrefs/**` (locks, loom artifacts).
//! Anything else is swept once it's older than the grace period — younger
//! blobs survive regardless, so a GC pass can never race an in-flight push
//! (blobs upload before the op that names them).

use std::time::Duration;

use lex_store::files::{Entry, Manifest};
use lex_store::DEFAULT_BRANCH as MAIN;
use lex_store::Store;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn head(s: &Store, branch: &str) -> Option<String> {
    s.get_branch(branch).unwrap().and_then(|b| b.head_op)
}

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

fn set_files(s: &Store, branch: &str, manifest: &str) -> String {
    s.apply_set_files(branch, manifest, None).unwrap()
}

/// No grace period: anything unreferenced right now is fair game. Used
/// where the test wants to assert "would be swept" without waiting real
/// wall-clock time for a blob to age past a nonzero grace period.
const NO_GRACE: Duration = Duration::from_secs(0);

/// A grace period comfortably longer than this test can possibly take,
/// for asserting "too young to sweep yet".
const LONG_GRACE: Duration = Duration::from_secs(24 * 3600);

#[test]
fn stale_unreferenced_blob_from_a_deleted_branch_is_swept() {
    let (s, _tmp) = fresh();
    // main: README = "kept".
    let kept_manifest = manifest_of(&s, &[("README.md", b"kept")]);
    set_files(&s, MAIN, &kept_manifest);

    // A second branch records a DIFFERENT manifest (different blob), then
    // gets deleted — its op (and the blob only it referenced) becomes
    // unreachable from any retained branch head.
    s.create_branch("throwaway", MAIN).unwrap();
    let orphan_manifest = manifest_of(&s, &[("README.md", b"orphaned content")]);
    let orphan_head = set_files(&s, "throwaway", &orphan_manifest);
    let orphan_blob = s.get_manifest(&orphan_manifest).unwrap().entries["README.md"].blob.clone();
    assert!(s.has_blob(&orphan_blob));
    s.delete_branch("throwaway").unwrap();
    let _ = orphan_head;

    let plan = s.plan_blob_gc(NO_GRACE).unwrap();
    assert!(plan.to_delete.contains(&orphan_manifest), "orphan manifest blob should be swept");
    assert!(plan.to_delete.contains(&orphan_blob), "orphan README blob should be swept");
    assert!(!plan.live.contains(&orphan_blob));

    let removed = s.apply_blob_gc(&plan).unwrap();
    assert_eq!(removed, plan.to_delete.len());
    assert!(!s.has_blob(&orphan_blob), "orphan blob must be gone from disk");
    assert!(!s.has_blob(&orphan_manifest));

    // The blob a retained head still needs must be untouched.
    let kept_blob = s.get_manifest(&kept_manifest).unwrap().entries["README.md"].blob.clone();
    assert!(s.has_blob(&kept_blob));
    assert!(s.has_blob(&kept_manifest));
}

#[test]
fn blob_referenced_by_a_retained_head_survives_even_with_no_grace() {
    let (s, _tmp) = fresh();
    let m = manifest_of(&s, &[("README.md", b"still here")]);
    set_files(&s, MAIN, &m);
    let blob = s.get_manifest(&m).unwrap().entries["README.md"].blob.clone();

    let plan = s.plan_blob_gc(NO_GRACE).unwrap();
    assert!(plan.live.contains(&blob), "blob reachable from main's head must be live");
    assert!(plan.live.contains(&m), "the manifest blob itself must be live");
    assert!(!plan.to_delete.contains(&blob));
    assert!(!plan.to_delete.contains(&m));

    let removed = s.apply_blob_gc(&plan).unwrap();
    assert_eq!(removed, 0);
    assert!(s.has_blob(&blob));
}

#[test]
fn blob_referenced_by_a_branch_other_than_main_survives() {
    let (s, _tmp) = fresh();
    // main carries no files at all.
    s.create_branch("feature", MAIN).unwrap();
    let m = manifest_of(&s, &[("NOTES.md", b"feature notes")]);
    set_files(&s, "feature", &m);
    let blob = s.get_manifest(&m).unwrap().entries["NOTES.md"].blob.clone();

    let plan = s.plan_blob_gc(NO_GRACE).unwrap();
    assert!(plan.live.contains(&blob), "reachable from a live non-default branch");
    let removed = s.apply_blob_gc(&plan).unwrap();
    assert_eq!(removed, 0);
    assert!(s.has_blob(&blob));
}

#[test]
fn blob_bound_under_blobrefs_survives_even_if_no_manifest_names_it() {
    let (s, _tmp) = fresh();
    // A committed lock: content-addressed via `put_blob` + bound under the
    // `lock` blobrefs namespace, keyed by an op id — no `SetFiles`/manifest
    // is involved at all.
    s.set_committed_lock("some-head-op", "version = 1\n").unwrap();
    let lock_sha = s.get_blob_ref("lock", "some-head-op").unwrap();
    assert!(s.has_blob(&lock_sha));

    let plan = s.plan_blob_gc(NO_GRACE).unwrap();
    assert!(plan.live.contains(&lock_sha), "blobrefs entries are always live");
    assert!(!plan.to_delete.contains(&lock_sha));

    let removed = s.apply_blob_gc(&plan).unwrap();
    assert_eq!(removed, 0);
    assert!(s.has_blob(&lock_sha));
}

#[test]
fn fresh_unreferenced_blob_within_grace_period_survives() {
    let (s, _tmp) = fresh();
    // A blob nothing references, uploaded "just now" (this instant) — the
    // race the grace period exists for: uploaded before the op naming it
    // has landed. A long grace period must not sweep it.
    let fresh_blob = s.put_blob_bytes(b"about to be named by an in-flight push").unwrap();
    assert!(s.has_blob(&fresh_blob));

    let plan = s.plan_blob_gc(LONG_GRACE).unwrap();
    assert!(!plan.live.contains(&fresh_blob), "nothing references it yet");
    assert!(!plan.to_delete.contains(&fresh_blob), "but it's too young to sweep");
    assert!(plan.skipped_within_grace.contains(&fresh_blob));

    let removed = s.apply_blob_gc(&plan).unwrap();
    assert_eq!(removed, 0);
    assert!(s.has_blob(&fresh_blob), "must survive the grace window");
}

#[test]
fn running_gc_twice_in_a_row_is_idempotent() {
    let (s, _tmp) = fresh();
    let kept_manifest = manifest_of(&s, &[("README.md", b"kept")]);
    set_files(&s, MAIN, &kept_manifest);
    s.create_branch("throwaway", MAIN).unwrap();
    let orphan_manifest = manifest_of(&s, &[("README.md", b"gone soon")]);
    set_files(&s, "throwaway", &orphan_manifest);
    s.delete_branch("throwaway").unwrap();

    let first = s.plan_blob_gc(NO_GRACE).unwrap();
    assert!(!first.is_empty(), "the orphaned blob should be scheduled for deletion");
    let first_removed = s.apply_blob_gc(&first).unwrap();
    assert_eq!(first_removed, first.to_delete.len());

    // Second pass over the now-cleaned store: nothing left to delete.
    let second = s.plan_blob_gc(NO_GRACE).unwrap();
    assert!(second.is_empty(), "a second pass must sweep nothing new: {:?}", second.to_delete);
    let second_removed = s.apply_blob_gc(&second).unwrap();
    assert_eq!(second_removed, 0);

    // The retained blob from the first pass is still there after both.
    let kept_blob = s.get_manifest(&kept_manifest).unwrap().entries["README.md"].blob.clone();
    assert!(s.has_blob(&kept_blob));
    let _ = head(&s, MAIN);
}
