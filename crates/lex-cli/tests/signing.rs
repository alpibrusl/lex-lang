//! End-to-end tests for `lex keygen`, `lex publish --signing-key`,
//! and `lex store get --require-signed/--trusted-key` (#227).
//!
//! The signing story is small but layered: keygen produces fresh
//! hex key material (deterministic length, random content); publish
//! persists the signature into the stage's metadata; store get
//! verifies signatures and rejects mismatches. The tests below pin
//! the full chain — author signs, store verifies, tamper detected,
//! wrong-key detected, unsigned-when-required detected.

use std::process::{Command, Stdio};
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

const SOURCE: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";

// ---- keygen ------------------------------------------------------

#[test]
fn keygen_produces_64_hex_char_keys() {
    let (code, stdout, _) = run(&["--output", "json", "keygen"]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let pk = v.pointer("/data/public_key")
        .or_else(|| v.get("public_key"))
        .and_then(|x| x.as_str())
        .expect("public_key");
    let sk = v.pointer("/data/secret_key")
        .or_else(|| v.get("secret_key"))
        .and_then(|x| x.as_str())
        .expect("secret_key");
    assert_eq!(pk.len(), 64, "public key must be 64 hex chars");
    assert_eq!(sk.len(), 64, "secret key must be 64 hex chars");
    assert!(pk.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(sk.chars().all(|c| c.is_ascii_hexdigit()));
    // Public must differ from secret — a paranoia check that the
    // formatter didn't accidentally print the same field twice.
    assert_ne!(pk, sk);
}

#[test]
fn keygen_text_output_lists_both_keys() {
    let (code, stdout, _) = run(&["keygen"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("public_key"));
    assert!(stdout.contains("secret_key"));
}

#[test]
fn two_keygens_produce_distinct_keys() {
    let (_, a_out, _) = run(&["--output", "json", "keygen"]);
    let (_, b_out, _) = run(&["--output", "json", "keygen"]);
    let a: serde_json::Value = serde_json::from_str(&a_out).unwrap();
    let b: serde_json::Value = serde_json::from_str(&b_out).unwrap();
    let a_pk = a.pointer("/data/public_key").and_then(|x| x.as_str()).unwrap();
    let b_pk = b.pointer("/data/public_key").and_then(|x| x.as_str()).unwrap();
    assert_ne!(a_pk, b_pk, "keygen must produce fresh randomness each call");
}

// ---- publish + verify --------------------------------------------

/// Generate a keypair and return (public_hex, secret_hex).
fn fresh_keypair() -> (String, String) {
    let (_, out, _) = run(&["--output", "json", "keygen"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let pk = v.pointer("/data/public_key").and_then(|x| x.as_str()).unwrap().to_string();
    let sk = v.pointer("/data/secret_key").and_then(|x| x.as_str()).unwrap().to_string();
    (pk, sk)
}

/// Pull the first add_function stage_id out of the JSON envelope
/// returned by `lex publish`. Used so tests don't have to depend on
/// `--activate` having run.
fn first_stage_id(publish_json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(publish_json).unwrap();
    let ops = v.pointer("/data/ops")
        .or_else(|| v.get("ops"))
        .and_then(|x| x.as_array())
        .expect("ops array");
    for op in ops {
        if let Some(stg) = op.pointer("/kind/stage_id").and_then(|x| x.as_str()) {
            return stg.to_string();
        }
    }
    panic!("no stage_id found in publish output: {publish_json}");
}

/// Publish `SOURCE` under `--signing-key sk`, return the StageId
/// of the published function stage.
fn publish_signed(store_path: &std::path::Path, sk: &str) -> String {
    let src = store_path.join("a.lex");
    std::fs::write(&src, SOURCE).unwrap();
    let (code, stdout, stderr) = run(&[
        "--output", "json", "publish",
        "--store", store_path.to_str().unwrap(),
        "--signing-key", sk,
        src.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "publish failed; stderr: {stderr}");
    first_stage_id(&stdout)
}

fn publish_unsigned(store_path: &std::path::Path) -> String {
    let src = store_path.join("a.lex");
    std::fs::write(&src, SOURCE).unwrap();
    let (code, stdout, stderr) = run(&[
        "--output", "json", "publish",
        "--store", store_path.to_str().unwrap(),
        src.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "publish failed; stderr: {stderr}");
    first_stage_id(&stdout)
}

#[test]
fn publish_with_signing_key_persists_signature_in_metadata() {
    let store = tempdir().unwrap();
    let (pk, sk) = fresh_keypair();
    let stage_id = publish_signed(store.path(), &sk);

    let (code, stdout, _) = run(&[
        "--output", "json", "store", "get",
        "--store", store.path().to_str().unwrap(),
        &stage_id,
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let sig = v.pointer("/data/metadata/signature")
        .or_else(|| v.pointer("/metadata/signature"))
        .expect("metadata.signature should be set on signed publish");
    assert_eq!(sig["public_key"].as_str(), Some(pk.as_str()),
        "metadata public key must match the signer");
    assert_eq!(sig["signature"].as_str().unwrap().len(), 128,
        "Ed25519 signature is 64 bytes => 128 hex chars");
}

#[test]
fn publish_without_signing_key_persists_no_signature() {
    let store = tempdir().unwrap();
    let stage_id = publish_unsigned(store.path());
    let (_, get_out, _) = run(&[
        "--output", "json", "store", "get",
        "--store", store.path().to_str().unwrap(),
        &stage_id,
    ]);
    let g: serde_json::Value = serde_json::from_str(&get_out).unwrap();
    let sig = g.pointer("/data/metadata/signature")
        .or_else(|| g.pointer("/metadata/signature"));
    assert!(sig.is_none() || sig.unwrap().is_null(),
        "unsigned publish must not write a signature: got {sig:?}");
}

#[test]
fn store_get_require_signed_accepts_signed_stage() {
    let store = tempdir().unwrap();
    let (_, sk) = fresh_keypair();
    let stage_id = publish_signed(store.path(), &sk);

    let (code, _, stderr) = run(&[
        "--output", "json", "store", "get",
        "--store", store.path().to_str().unwrap(),
        "--require-signed",
        &stage_id,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
}

#[test]
fn store_get_require_signed_rejects_unsigned_stage() {
    let store = tempdir().unwrap();
    let stage_id = publish_unsigned(store.path());
    let (code, _, stderr) = run(&[
        "store", "get",
        "--store", store.path().to_str().unwrap(),
        "--require-signed",
        &stage_id,
    ]);
    assert_ne!(code, 0, "should refuse unsigned stage under --require-signed");
    assert!(stderr.contains("not signed"),
        "stderr should mention the missing signature; got: {stderr}");
}

#[test]
fn store_get_trusted_key_accepts_matching_signer() {
    let store = tempdir().unwrap();
    let (pk, sk) = fresh_keypair();
    let stage_id = publish_signed(store.path(), &sk);

    let (code, _, stderr) = run(&[
        "--output", "json", "store", "get",
        "--store", store.path().to_str().unwrap(),
        "--trusted-key", &pk,
        &stage_id,
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
}

#[test]
fn store_get_trusted_key_rejects_mismatched_signer() {
    // Stage was signed by `signer_a`; consumer demands `signer_b`.
    let store = tempdir().unwrap();
    let (_pk_a, sk_a) = fresh_keypair();
    let (pk_b, _sk_b) = fresh_keypair();
    let stage_id = publish_signed(store.path(), &sk_a);

    let (code, _, stderr) = run(&[
        "store", "get",
        "--store", store.path().to_str().unwrap(),
        "--trusted-key", &pk_b,
        &stage_id,
    ]);
    assert_ne!(code, 0, "wrong trusted key must be rejected");
    assert!(stderr.contains("not by trusted key") || stderr.contains("trusted"),
        "stderr should explain the trust mismatch; got: {stderr}");
}

#[test]
fn store_get_detects_tampered_signature() {
    // Publish, then corrupt the signature byte and re-read.
    let store = tempdir().unwrap();
    let (_, sk) = fresh_keypair();
    let stage_id = publish_signed(store.path(), &sk);

    // Find and rewrite the metadata file's `signature.signature` hex.
    let stages_dir = store.path().join("stages");
    let mut tampered = false;
    'outer: for sig_dir in std::fs::read_dir(&stages_dir).unwrap() {
        let imp = sig_dir.unwrap().path().join("implementations");
        if !imp.exists() { continue; }
        for f in std::fs::read_dir(&imp).unwrap() {
            let p = f.unwrap().path();
            if p.to_string_lossy().ends_with(".metadata.json") {
                let s = std::fs::read_to_string(&p).unwrap();
                let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
                if let Some(obj) = v.get_mut("signature").and_then(|s| s.as_object_mut()) {
                    let sig = obj.get_mut("signature").unwrap().as_str().unwrap().to_string();
                    // Flip one hex char so the signature is structurally
                    // valid (still 128 hex chars) but doesn't verify.
                    let mut bytes = sig.into_bytes();
                    bytes[10] = if bytes[10] == b'a' { b'b' } else { b'a' };
                    let bumped = String::from_utf8(bytes).unwrap();
                    obj.insert("signature".to_string(), serde_json::Value::String(bumped));
                    std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
                    tampered = true;
                    break 'outer;
                }
            }
        }
    }
    assert!(tampered, "test setup: failed to find a metadata file to tamper with");

    let (code, _, stderr) = run(&[
        "store", "get",
        "--store", store.path().to_str().unwrap(),
        "--require-signed",
        &stage_id,
    ]);
    assert_ne!(code, 0, "tampered signature must fail verification");
    assert!(stderr.contains("failed verification") || stderr.contains("verify"),
        "stderr should explain the verification failure; got: {stderr}");
}

#[test]
fn lex_signing_key_env_var_is_picked_up() {
    let store = tempdir().unwrap();
    let (pk, sk) = fresh_keypair();
    let src = store.path().join("a.lex");
    std::fs::write(&src, SOURCE).unwrap();

    let out = Command::new(lex_bin())
        .args([
            "--output", "json", "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .env("LEX_SIGNING_KEY", &sk)
        .output()
        .expect("spawn");
    assert!(out.status.success(),
        "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let publish_stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stage_id = first_stage_id(&publish_stdout);
    let (_, get_out, _) = run(&[
        "--output", "json", "store", "get",
        "--store", store.path().to_str().unwrap(),
        "--trusted-key", &pk,
        &stage_id,
    ]);
    let g: serde_json::Value = serde_json::from_str(&get_out).unwrap();
    let sig_pk = g.pointer("/data/metadata/signature/public_key")
        .or_else(|| g.pointer("/metadata/signature/public_key"))
        .and_then(|x| x.as_str())
        .expect("env-var-signed publish must persist signature");
    assert_eq!(sig_pk, pk);
}

#[test]
fn invalid_signing_key_hex_surfaces_clear_error() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, SOURCE).unwrap();
    let (code, _, stderr) = run(&[
        "publish",
        "--store", store.path().to_str().unwrap(),
        "--signing-key", "not-hex",
        src.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("invalid signing key") ||
            stderr.to_lowercase().contains("hex"),
        "stderr should mention the bad hex; got: {stderr}");
}
