//! Conformance for `lex attest import-install` (lex-os#36 / #38).
//!
//! Promotes a lex-os capsule-install audit log into the durable
//! attestation graph, and proves the loop it closes: the imported
//! `CapsuleInstall` records are signer-keyed, so `producer-trust` scores
//! the signer and the trusted-keys keyring exports it.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

const SIGNER: &str = "f9b439837a3d0c1e2b4a5c6d7e8f90112233445566778899aabbccddeeff0011";

/// A minimal lex-os audit log (the shape `AuditLog::to_json()` writes):
/// an array of `{seq, prev_hash, event, hash}` entries. The importer only
/// reads `event`; one install is wrapped in request/refuse noise to prove
/// it imports exactly the `capsule_installed` events.
fn sample_audit_log() -> String {
    serde_json::json!([
        {
            "seq": 0,
            "prev_hash": "",
            "event": { "kind": "capsule_requested", "artifact": "pdf-extract@2.0.0", "signer": SIGNER },
            "hash": "aaa"
        },
        {
            "seq": 1,
            "prev_hash": "aaa",
            "event": {
                "kind": "capsule_installed",
                "artifact": "pdf-extract@2.0.0",
                "signer": SIGNER,
                "content_hash": "deadbeefcafe0000111122223333444455556666777788889999aaaabbbbcccc",
                "effective_grant": "fs=read-only net=allowlist exec=none"
            },
            "hash": "bbb"
        },
        {
            "seq": 2,
            "prev_hash": "bbb",
            "event": { "kind": "capsule_refused", "artifact": "evil@0.1.0", "reason": "untrusted signer" },
            "hash": "ccc"
        }
    ])
    .to_string()
}

#[test]
fn import_install_promotes_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let audit = tmp.path().join("install.audit.json");
    std::fs::write(&audit, sample_audit_log()).unwrap();

    let import = || {
        let out = Command::new(lex_bin())
            .args([
                "--output",
                "json",
                "attest",
                "import-install",
                "--audit",
                audit.to_str().unwrap(),
                "--store",
                store.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };

    // First import: exactly the one `capsule_installed` event lands.
    let first = import();
    assert_eq!(first["data"]["imported"], 1, "one install event imported");
    assert_eq!(first["data"]["already_present"], 0);
    let att = &first["data"]["attestations"][0];
    assert_eq!(att["signer"], SIGNER);
    assert_eq!(att["already_present"], false);

    // Second import of the same log: content-addressed dedup → no new fact.
    let second = import();
    assert_eq!(second["data"]["imported"], 1);
    assert_eq!(
        second["data"]["already_present"], 1,
        "re-import dedups by content-addressed id"
    );
}

#[test]
fn imported_installs_are_blame_queryable_and_feed_producer_trust() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let audit = tmp.path().join("install.audit.json");
    std::fs::write(&audit, sample_audit_log()).unwrap();

    let run = |args: &[&str]| {
        let out = Command::new(lex_bin()).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "args={args:?} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let store_s = store.to_str().unwrap();

    run(&[
        "--output",
        "json",
        "attest",
        "import-install",
        "--audit",
        audit.to_str().unwrap(),
        "--store",
        store_s,
    ]);

    // Queryable by kind, keyed under the signer's stage.
    let filtered = run(&[
        "--output",
        "json",
        "attest",
        "filter",
        "--kind",
        "capsule_install",
        "--store",
        store_s,
    ]);
    assert_eq!(filtered["data"]["count"], 1);
    assert_eq!(
        filtered["data"]["attestations"][0]["stage_id"], SIGNER,
        "the install attestation is keyed under the signer"
    );
    assert_eq!(
        filtered["data"]["attestations"][0]["produced_by"]["tool"], SIGNER,
        "produced_by.tool == signer so producer-trust scores the publisher"
    );

    // The install is earned track record: recompute trust for the signer,
    // then the keyring exports it — the loop #626's keyring consumes.
    run(&[
        "--output",
        "json",
        "producer-trust",
        "recompute",
        "--tool",
        SIGNER,
        "--store",
        store_s,
    ]);
    let keyring = run(&[
        "producer-trust",
        "keyring",
        "--store",
        store_s,
        "--min-trust",
        "700",
    ]);
    let trusted: Vec<&str> = keyring["trusted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        trusted,
        vec![SIGNER],
        "a clean install earns the signer a place in the trusted-keys keyring"
    );
}
