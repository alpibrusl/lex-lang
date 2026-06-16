//! Generic content-addressed blob store + ref namespace (#5 / M6.1a).
//!
//! The blob sha must equal Lex's `crypto.sha256_str(content)` so blobs are
//! interchangeable with loom's content-addressed SQLite artifacts.

use lex_store::{Store, StoreError};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

#[test]
fn put_blob_sha_matches_crypto_sha256_str() {
    let (s, _tmp) = fresh();
    let sha = s.put_blob("hello world").unwrap();
    // sha256("hello world"), lowercase hex — the same value Lex's
    // crypto.sha256_str produces and that loom's M6.0 SQLite store keys on.
    assert_eq!(
        sha,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
    assert_eq!(s.get_blob(&sha).unwrap(), "hello world");
}

#[test]
fn put_blob_is_idempotent_and_dedups() {
    let (s, tmp) = fresh();
    let a = s.put_blob("same content").unwrap();
    let b = s.put_blob("same content").unwrap();
    assert_eq!(a, b, "identical content yields identical sha");
    // exactly one file on disk for the deduped content
    let count = std::fs::read_dir(tmp.path().join("blobs"))
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_file())
        .count();
    assert_eq!(count, 1, "duplicate content must not create a second blob");
}

#[test]
fn distinct_content_distinct_sha() {
    let (s, _tmp) = fresh();
    assert_ne!(s.put_blob("a").unwrap(), s.put_blob("b").unwrap());
}

#[test]
fn get_blob_unknown_errors() {
    let (s, _tmp) = fresh();
    assert!(matches!(
        s.get_blob("deadbeef"),
        Err(StoreError::UnknownBlob(_))
    ));
    assert!(!s.has_blob("deadbeef"));
}

#[test]
fn blob_refs_round_trip_and_list() {
    let (s, _tmp) = fresh();
    let ns = "loom/sprint-abc";
    let h_build = s.put_blob("build output").unwrap();
    let h_qa = s.put_blob("qa verdict").unwrap();
    s.set_blob_ref(ns, "build-node", &h_build).unwrap();
    s.set_blob_ref(ns, "qa-node", &h_qa).unwrap();

    assert_eq!(s.get_blob_ref(ns, "build-node").unwrap(), h_build);
    assert_eq!(s.get_blob_ref(ns, "qa-node").unwrap(), h_qa);

    let all = s.list_blob_refs(ns).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all.get("build-node"), Some(&h_build));

    // a different sprint namespace is isolated
    assert!(s.list_blob_refs("loom/sprint-other").unwrap().is_empty());
}

#[test]
fn blob_ref_overwrite() {
    let (s, _tmp) = fresh();
    let h1 = s.put_blob("v1").unwrap();
    let h2 = s.put_blob("v2").unwrap();
    s.set_blob_ref("ns", "k", &h1).unwrap();
    s.set_blob_ref("ns", "k", &h2).unwrap();
    assert_eq!(s.get_blob_ref("ns", "k").unwrap(), h2);
}

#[test]
fn unknown_ref_errors() {
    let (s, _tmp) = fresh();
    assert!(matches!(
        s.get_blob_ref("ns", "missing"),
        Err(StoreError::UnknownBlobRef { .. })
    ));
}

#[test]
fn path_traversal_is_rejected() {
    let (s, _tmp) = fresh();
    let h = s.put_blob("x").unwrap();
    assert!(s.set_blob_ref("ns", "../escape", &h).is_err());
    assert!(s.set_blob_ref("../escape", "k", &h).is_err());
    assert!(s.set_blob_ref("ns", "a/b", &h).is_err());
}
