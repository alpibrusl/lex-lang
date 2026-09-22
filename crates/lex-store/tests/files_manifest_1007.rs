//! #1007 PR 1: binary blobs + the files `Manifest`.
//!
//! Golden values below were computed outside Rust (`shasum -a 256` / Python
//! `hashlib`) so a change to the encoder cannot silently move them.

use std::collections::BTreeMap;

use lex_store::files::{is_reserved_path, validate_path, Entry, Manifest, ManifestError};
use lex_store::{Store, StoreError};

const HELLO_SHA: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn entry(blob: &str, mode: &str, size: u64) -> Entry {
    Entry {
        blob: blob.into(),
        mode: mode.into(),
        size,
    }
}

fn manifest(paths: &[&str]) -> Manifest {
    let mut m = Manifest::new();
    for p in paths {
        m.entries
            .insert(p.to_string(), entry(HELLO_SHA, "100644", 11));
    }
    m
}

// ── binary blobs ─────────────────────────────────────────────────────────────

#[test]
fn non_utf8_blob_round_trips_with_sha256sum_id() {
    let (s, tmp) = fresh();
    // NUL, 0xFF/0xFE, a lone continuation byte, an invalid 2-byte sequence,
    // a newline and a PNG-ish tail — none of it is UTF-8.
    let bytes: Vec<u8> = vec![
        0x00, 0xff, 0xfe, 0x80, 0xc3, 0x28, 0x0a, 0x89, 0x50, 0x4e, 0x47,
    ];
    let bytes = bytes.as_slice();
    assert!(std::str::from_utf8(bytes).is_err());
    let id = s.put_blob_bytes(bytes).unwrap();
    // `printf '\x00\xff\xfe\x80\xc3\x28\x0a\x89\x50\x4e\x47' | shasum -a 256`
    assert_eq!(
        id,
        "8d2345c07dc80b216f98421970e018f056b38a375b4d77001517a2dfadcda3f8"
    );
    assert_eq!(s.get_blob_bytes(&id).unwrap(), bytes);
    // Exact bytes on disk, no transcoding.
    assert_eq!(
        std::fs::read(tmp.path().join("blobs").join(&id)).unwrap(),
        bytes
    );
    // The text API refuses it rather than lossily decoding.
    assert!(matches!(s.get_blob(&id), Err(StoreError::Io(_))));
    assert!(s.has_blob(&id));
}

#[test]
fn text_blob_id_is_unchanged_and_apis_agree() {
    let (s, _tmp) = fresh();
    let via_text = s.put_blob("hello world").unwrap();
    assert_eq!(via_text, HELLO_SHA);
    assert_eq!(s.put_blob_bytes(b"hello world").unwrap(), HELLO_SHA);
    assert_eq!(s.get_blob_bytes(HELLO_SHA).unwrap(), b"hello world");
    assert_eq!(s.get_blob(HELLO_SHA).unwrap(), "hello world");
}

#[test]
fn blob_reads_refuse_non_ids() {
    let (s, tmp) = fresh();
    // A file outside blobs/ that a traversal would reach.
    std::fs::write(tmp.path().join("secret"), b"x").unwrap();
    s.put_blob("anything").unwrap();
    for bad in ["../secret", "deadbeef", &HELLO_SHA.to_uppercase()] {
        assert!(
            matches!(s.get_blob_bytes(bad), Err(StoreError::UnknownBlob(_))),
            "{bad}"
        );
        assert!(!s.has_blob(bad), "{bad}");
    }
}

// ── manifest encoding ────────────────────────────────────────────────────────

#[test]
fn canonical_manifest_id_golden() {
    let mut m = Manifest::new();
    // Inserted out of order: the BTreeMap sorts, so insertion order is moot.
    m.entries
        .insert("bin/run".into(), entry(HELLO_SHA, "100755", 11));
    m.entries
        .insert("README.md".into(), entry(HELLO_SHA, "100644", 11));
    let expected = format!(
        r#"{{"version":1,"entries":{{"README.md":{{"blob":"{h}","mode":"100644","size":11}},"bin/run":{{"blob":"{h}","mode":"100755","size":11}}}}}}"#,
        h = HELLO_SHA
    );
    assert_eq!(String::from_utf8(m.to_canonical_bytes()).unwrap(), expected);
    assert_eq!(
        m.id(),
        "dd78532ecbfda2ce0b0d40be6ded96c79bd065580de47f7c6a05664d0226566b"
    );

    // Stored as a blob under that id, and read back equal.
    let (s, _tmp) = fresh();
    let id = s.put_manifest(&m).unwrap();
    assert_eq!(id, m.id());
    assert_eq!(s.get_manifest(&id).unwrap(), m);
}

#[test]
fn only_the_canonical_encoding_decodes() {
    let m = manifest(&["README.md"]);
    let canon = m.to_canonical_bytes();
    assert_eq!(Manifest::from_bytes(&canon).unwrap(), m);

    let pretty = serde_json::to_vec_pretty(&m).unwrap();
    assert_eq!(
        Manifest::from_bytes(&pretty),
        Err(ManifestError::NotCanonical)
    );

    let reordered = format!(
        r#"{{"entries":{{"README.md":{{"blob":"{HELLO_SHA}","mode":"100644","size":11}}}},"version":1}}"#
    );
    assert_eq!(
        Manifest::from_bytes(reordered.as_bytes()),
        Err(ManifestError::NotCanonical)
    );

    let extra = br#"{"version":1,"entries":{},"x":1}"#;
    assert!(matches!(
        Manifest::from_bytes(extra),
        Err(ManifestError::Malformed(_))
    ));
    assert!(matches!(
        Manifest::from_bytes(b"\xff"),
        Err(ManifestError::Malformed(_))
    ));
    assert!(matches!(
        Manifest::from_bytes(br#"{"version":2,"entries":{}}"#),
        Err(ManifestError::UnsupportedVersion(2))
    ));
}

// ── path validation ──────────────────────────────────────────────────────────

#[test]
fn rejected_path_table() {
    // (path, why) — each must be refused on its own.
    let rejected: &[(&str, &str)] = &[
        ("", "empty"),
        ("/etc/passwd", "leading /"),
        ("a\\b", "backslash"),
        ("a\0b", "NUL"),
        ("..", "dotdot"),
        ("a/../b", "dotdot component"),
        ("./a", "dot component"),
        ("a//b", "empty component"),
        ("a/", "trailing slash"),
        (".git", ".git"),
        (".git/config", ".git component"),
        ("sub/.git/HEAD", "nested .git"),
        (".GIT/config", ".git any case"),
        (".lex/ops/x", "store dir"),
        ("src.lex", "reserved root module"),
        ("src/main.lex", "reserved src/*.lex"),
        ("src/a/b/c.lex", "reserved src/**/*.lex"),
    ];
    for (p, why) in rejected {
        assert!(
            validate_path(p).is_err(),
            "`{}` must be rejected ({why})",
            p.escape_debug()
        );
        // And a manifest containing it is refused too.
        assert!(
            manifest(&[p]).validate().is_err(),
            "manifest with `{}` ({why})",
            p.escape_debug()
        );
    }
    assert!(matches!(
        validate_path("src/main.lex"),
        Err(ManifestError::ReservedPath(_))
    ));
}

#[test]
fn accepted_path_table() {
    // Negative control for the table above: near-misses that ARE blobs.
    for p in [
        "README.md",
        "lex.toml",
        "lex.lock",
        "src/README.md", // non-.lex under src/ is a blob
        "src/data.json",
        "src.lex.bak",
        "tests/main.lex", // tests/ and examples/ are blobs (decided)
        "examples/demo.lex",
        "docs/src/x.lex", // only a top-level src/ is reserved
        ".github/workflows/ci.yml",
        ".gitignore", // a .git-prefixed name is not a .git component
        "a/.lexrc",
        "sub/.lex/x", // only a top-level .lex is the store
        "..foo",
        "héllo/ñ.txt",
    ] {
        assert_eq!(validate_path(p), Ok(()), "`{p}` must be accepted");
        assert!(!is_reserved_path(p), "{p}");
    }
    assert_eq!(
        manifest(&["README.md", "src/README.md", "tests/main.lex"]).validate(),
        Ok(())
    );
}

#[test]
fn cross_entry_and_entry_field_rules() {
    assert!(matches!(
        manifest(&["README.md", "readme.md"]).validate(),
        Err(ManifestError::CaseCollision(..))
    ));
    assert!(matches!(
        manifest(&["Docs/a.md", "docs/A.md"]).validate(),
        Err(ManifestError::CaseCollision(..))
    ));
    assert!(matches!(
        manifest(&["docs", "docs/a.md"]).validate(),
        Err(ManifestError::FileDirCollision(..))
    ));
    // Negative control: distinct names that merely share a prefix.
    assert_eq!(manifest(&["docs", "docs.md", "docsx/a"]).validate(), Ok(()));

    for mode in ["120000", "160000", "100664", "644", ""] {
        let mut m = Manifest::new();
        m.entries.insert("a".into(), entry(HELLO_SHA, mode, 11));
        assert!(
            matches!(m.validate(), Err(ManifestError::InvalidMode { .. })),
            "{mode}"
        );
    }
    for mode in ["100644", "100755"] {
        let mut m = Manifest::new();
        m.entries.insert("a".into(), entry(HELLO_SHA, mode, 11));
        assert_eq!(m.validate(), Ok(()), "{mode}");
    }
    let mut m = Manifest::new();
    m.entries.insert("a".into(), entry("../../x", "100644", 1));
    assert!(matches!(
        m.validate(),
        Err(ManifestError::InvalidBlobId { .. })
    ));

    let (s, _tmp) = fresh();
    assert!(matches!(
        s.put_manifest(&manifest(&["src/main.lex"])),
        Err(StoreError::InvalidManifest(ManifestError::ReservedPath(_)))
    ));
}

#[test]
fn closure_missing_lists_absent_entry_blobs() {
    let (s, _tmp) = fresh();
    let present = s.put_blob_bytes(b"present").unwrap();
    let absent = "0".repeat(64);
    let mut entries = BTreeMap::new();
    entries.insert("a".to_string(), entry(&present, "100644", 7));
    entries.insert("b".to_string(), entry(&absent, "100644", 1));
    entries.insert("c".to_string(), entry(&absent, "100755", 1)); // deduped
    let m = Manifest {
        version: 1,
        entries,
    };
    assert_eq!(s.manifest_closure_missing(&m), vec![absent.clone()]);

    s.put_blob_bytes(b"\x00").unwrap(); // unrelated blob changes nothing
    assert_eq!(s.manifest_closure_missing(&m), vec![absent]);
    assert!(s.manifest_closure_missing(&manifest(&[])).is_empty());
}
