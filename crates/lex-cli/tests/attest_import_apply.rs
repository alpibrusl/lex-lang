//! Conformance for `lex attest import-apply` (#790).
//!
//! Promotes a capability gate's decisions into the durable attestation
//! graph and proves the loop they close: the records are signer-keyed,
//! so `producer-trust` scores the submitter and the keyring exports it.
//!
//! Two gates are exercised from the same command — an infrastructure
//! plan gate and a Kubernetes admission gate — because the point of
//! `PlanApply` being one kind rather than two is that both promote
//! through this path without lex-lang learning either vocabulary.

use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

/// A pipeline identity whose plans are accepted.
const CLEAN: &str = "1111111111111111111111111111111111111111111111111111111111111111";
/// A submitter that gets refused as often as it succeeds.
const SLOPPY: &str = "2222222222222222222222222222222222222222222222222222222222222222";

const PLAN_A: &str = "aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff66667777888899990000";
const PLAN_B: &str = "bbbb1111cccc2222dddd3333eeee4444ffff5555000066667777888899991111";
const PLAN_C: &str = "cccc1111dddd2222eeee3333ffff4444000055556666777788889999aaaa2222";

fn entry(seq: u64, event: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "seq": seq, "prev_hash": "", "event": event, "hash": "x" })
}

/// The shape a `lex-iac` audit chain writes: request / charge noise
/// around the decisions, so the importer has to select rather than
/// promote everything it sees.
fn iac_audit_log() -> String {
    serde_json::json!([
        entry(
            0,
            serde_json::json!({
                "kind": "plan_requested",
                "artifact_sha256": PLAN_A, "manifest": "mf-payments", "signer": CLEAN,
            })
        ),
        entry(
            1,
            serde_json::json!({
                "kind": "plan_accepted",
                "artifact_sha256": PLAN_A, "manifest": "mf-payments",
                "signer": CLEAN, "subject": "payments/prod",
            })
        ),
        entry(
            2,
            serde_json::json!({
                "kind": "spend_charged",
                "artifact_sha256": PLAN_B, "manifest": "mf-payments", "signer": SLOPPY,
            })
        ),
        entry(
            3,
            serde_json::json!({
                "kind": "plan_accepted",
                "artifact_sha256": PLAN_B, "manifest": "mf-payments",
                "signer": SLOPPY, "subject": "payments/staging",
            })
        ),
        entry(
            4,
            serde_json::json!({
                "kind": "plan_refused",
                "artifact_sha256": PLAN_C, "manifest": "mf-payments",
                "signer": SLOPPY, "subject": "payments/prod",
                "reason": "aws.rds.delete destroys stateful infrastructure",
            })
        ),
    ])
    .to_string()
}

fn write(dir: &std::path::Path, name: &str, body: String) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn run_json(args: &[&str]) -> serde_json::Value {
    let out = Command::new(lex_bin()).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "args={args:?}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn run_fails(args: &[&str]) -> String {
    let out = Command::new(lex_bin()).args(args).output().unwrap();
    assert!(
        !out.status.success(),
        "args={args:?} unexpectedly succeeded:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The whole loop, on one log: select the decisions, key them under the
/// signer, dedup on re-import, and let producer-trust separate a
/// submitter that gets refused from one that doesn't.
#[test]
fn import_apply_promotes_both_verdicts_and_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let store_s = store.to_str().unwrap();
    let audit = write(tmp.path(), "iac.audit.json", iac_audit_log());
    let audit_s = audit.to_str().unwrap();

    let import = || {
        run_json(&[
            "--output",
            "json",
            "attest",
            "import-apply",
            "--audit",
            audit_s,
            "--gate",
            "terraform",
            "--accepted",
            "plan_accepted",
            "--refused",
            "plan_refused",
            "--store",
            store_s,
        ])
    };

    let first = import();
    assert_eq!(first["data"]["imported"], 3, "two accepted, one refused");
    assert_eq!(first["data"]["accepted"], 2);
    assert_eq!(first["data"]["refused"], 1);
    assert_eq!(
        first["data"]["already_present"], 0,
        "nothing was in the store yet"
    );

    // The refusal is on the record as a failure, not omitted.
    let refused = first["data"]["attestations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["artifact_sha256"] == PLAN_C)
        .expect("the refused plan was imported too");
    assert_eq!(refused["result"], "failed");
    assert_eq!(refused["signer"], SLOPPY);

    // Re-import of the same log mints no new facts.
    let second = import();
    assert_eq!(second["data"]["imported"], 3);
    assert_eq!(
        second["data"]["already_present"], 3,
        "content-addressed ids dedup a re-import"
    );

    // Keyed under the signer in both indices, or producer-trust never
    // sees them.
    let filtered = run_json(&[
        "--output",
        "json",
        "attest",
        "filter",
        "--kind",
        "plan_apply",
        "--store",
        store_s,
    ]);
    assert_eq!(filtered["data"]["count"], 3);
    for a in filtered["data"]["attestations"].as_array().unwrap() {
        assert_eq!(
            a["stage_id"], a["produced_by"]["tool"],
            "stage_id == produced_by.tool == signer is the keying convention"
        );
        assert_eq!(a["kind"]["kind"], "plan_apply");
        assert_eq!(a["kind"]["gate"], "terraform");
    }

    // ...and the scores separate the two submitters. The clean one is
    // 2 of 2; the sloppy one is 1 of 2. Without importing refusals both
    // would read 1000 and the signal would be worthless.
    for signer in [CLEAN, SLOPPY] {
        run_json(&[
            "--output",
            "json",
            "producer-trust",
            "recompute",
            "--tool",
            signer,
            "--store",
            store_s,
        ]);
    }
    let keyring = run_json(&[
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
        vec![CLEAN],
        "a submitter whose plans get refused does not earn the keyring"
    );
}

/// The same command, a different gate's vocabulary, no lex-lang change.
/// That is the whole argument for `PlanApply` over `InfraApply`.
#[test]
fn a_second_gate_promotes_through_the_same_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("store");
    let store_s = store.to_str().unwrap();
    // A Kubernetes admission log — different event kinds, and the
    // submitter identity comes from --signer rather than the events.
    let log = serde_json::json!([
        entry(
            0,
            serde_json::json!({ "kind": "pod_requested", "uid": "u1" })
        ),
        entry(
            1,
            serde_json::json!({
                "kind": "pod_admitted",
                "artifact_sha256": PLAN_A, "manifest": "mf-payments",
                "subject": "payments/exporter-7d9f-",
            })
        ),
    ])
    .to_string();
    let audit = write(tmp.path(), "k8s.audit.json", log);

    let out = run_json(&[
        "--output",
        "json",
        "attest",
        "import-apply",
        "--audit",
        audit.to_str().unwrap(),
        "--gate",
        "kubernetes",
        "--accepted",
        "pod_admitted",
        "--signer",
        "system:serviceaccount:payments:deployer",
        "--store",
        store_s,
    ]);
    assert_eq!(out["data"]["imported"], 1);
    let a = &out["data"]["attestations"][0];
    assert_eq!(a["signer"], "system:serviceaccount:payments:deployer");
    assert_eq!(a["subject"], "payments/exporter-7d9f-");

    let filtered = run_json(&[
        "--output",
        "json",
        "attest",
        "filter",
        "--kind",
        "plan_apply",
        "--store",
        store_s,
    ]);
    assert_eq!(filtered["data"]["count"], 1);
    assert_eq!(
        filtered["data"]["attestations"][0]["kind"]["gate"], "kubernetes",
        "the gate lives in the payload, not in a second AttestationKind"
    );
}

/// Nothing matched and the gate decided nothing are different facts,
/// and a caller who named the wrong event kind must not read the first
/// as the second.
#[test]
fn naming_the_wrong_event_kind_says_what_the_log_contains() {
    let tmp = tempfile::tempdir().unwrap();
    let audit = write(tmp.path(), "iac.audit.json", iac_audit_log());
    let out = run_json(&[
        "--output",
        "json",
        "attest",
        "import-apply",
        "--audit",
        audit.to_str().unwrap(),
        "--gate",
        "terraform",
        "--accepted",
        "plan_applied", // not a kind this log contains
        "--store",
        tmp.path().join("store").to_str().unwrap(),
    ]);
    assert_eq!(out["data"]["imported"], 0);
    let kinds: Vec<&str> = out["data"]["event_kinds_present"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"plan_accepted"),
        "the kinds actually present are reported: {kinds:?}"
    );
}

/// A decision that cannot be fully attributed is refused, never
/// imported half-formed. Each field is checked on its own so the test
/// cannot pass for the wrong reason.
#[test]
fn an_unattributable_decision_is_refused_rather_than_imported() {
    let tmp = tempfile::tempdir().unwrap();
    let store_s = tmp.path().join("store");
    let store_s = store_s.to_str().unwrap();

    let cases: [(&str, serde_json::Value, &str); 5] = [
        (
            "no_hash",
            serde_json::json!({ "kind": "plan_accepted", "manifest": "m", "signer": CLEAN }),
            "artifact_sha256",
        ),
        (
            "short_hash",
            serde_json::json!({
                "kind": "plan_accepted", "artifact_sha256": "abc123",
                "manifest": "m", "signer": CLEAN,
            }),
            "not a lowercase-hex SHA-256",
        ),
        (
            "no_manifest",
            serde_json::json!({
                "kind": "plan_accepted", "artifact_sha256": PLAN_A, "signer": CLEAN,
            }),
            "manifest",
        ),
        (
            "no_signer",
            serde_json::json!({
                "kind": "plan_accepted", "artifact_sha256": PLAN_A, "manifest": "m",
            }),
            "not evidence about anyone",
        ),
        (
            "not_an_array",
            serde_json::json!({ "entries": [] }),
            "not a JSON array",
        ),
    ];

    for (name, event, expected) in cases {
        let body = if name == "not_an_array" {
            event.to_string()
        } else {
            serde_json::json!([entry(0, event)]).to_string()
        };
        let audit = write(tmp.path(), &format!("{name}.json"), body);
        let stderr = run_fails(&[
            "attest",
            "import-apply",
            "--audit",
            audit.to_str().unwrap(),
            "--gate",
            "terraform",
            "--accepted",
            "plan_accepted",
            "--store",
            store_s,
        ]);
        assert!(
            stderr.contains(expected),
            "{name}: expected `{expected}` in stderr, got:\n{stderr}"
        );
    }
}

/// An event that names its own signer, and a `--signer` that disagrees,
/// is a re-attribution. Refuse it rather than silently letting one
/// identity's track record land on another.
#[test]
fn a_signer_flag_that_contradicts_the_log_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let audit = write(tmp.path(), "iac.audit.json", iac_audit_log());
    let stderr = run_fails(&[
        "attest",
        "import-apply",
        "--audit",
        audit.to_str().unwrap(),
        "--gate",
        "terraform",
        "--accepted",
        "plan_accepted",
        "--signer",
        SLOPPY,
        "--store",
        tmp.path().join("store").to_str().unwrap(),
    ]);
    assert!(
        stderr.contains("refusing to re-attribute"),
        "got:\n{stderr}"
    );
}

/// Naming one kind as both verdicts would import every acceptance as a
/// pass *and* a failure, halving a signer's score for succeeding.
#[test]
fn one_event_kind_cannot_be_both_verdicts() {
    let tmp = tempfile::tempdir().unwrap();
    let audit = write(tmp.path(), "iac.audit.json", iac_audit_log());
    let stderr = run_fails(&[
        "attest",
        "import-apply",
        "--audit",
        audit.to_str().unwrap(),
        "--gate",
        "terraform",
        "--accepted",
        "plan_accepted",
        "--refused",
        "plan_accepted",
        "--store",
        tmp.path().join("store").to_str().unwrap(),
    ]);
    assert!(stderr.contains("same event kind"), "got:\n{stderr}");
}
