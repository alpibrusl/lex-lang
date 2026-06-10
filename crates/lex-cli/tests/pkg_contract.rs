//! Conformance for `lex pkg publish --sign` + `lex pkg verify` — the registry
//! leg of lex-os#36: a published package carries a signed capability contract
//! (the lex-os capsule format) binding its bytes to the grant it needs, and the
//! package manager verifies that contract before trusting a dependency.
//!
//! Byte-compatibility with `lex-os capsule install` is locked by
//! `capsule_contract::tests::canonical_form_is_pinned` (the v1 golden vector);
//! lex-os's binary isn't available here, so these tests cover lex-lang's own
//! publish/verify surface and its three refusal paths.

use std::path::Path;
use std::process::Command;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

/// A fixed 32-byte seed (64 hex) so the signer key is deterministic.
const SECRET: &str = "1122334455667788990011223344556677889900112233445566778899001122";

fn signer_pubkey() -> String {
    lex_vcs::Keypair::from_secret_hex(SECRET).unwrap().public_hex()
}

fn setup_pkg(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("lex.toml"),
        "[package]\nname = \"lex-weather\"\nversion = \"1.2.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.lex"),
        "fn main() -> [net(\"api.weather.example\")] Unit { net.fetch(\"api.weather.example\") }\n",
    )
    .unwrap();
}

/// Publish with a signed contract; returns (contract_path, archive_path).
fn publish_signed(work: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let pkg = work.join("pkg");
    setup_pkg(&pkg);
    let grant = work.join("grant.json");
    std::fs::write(
        &grant,
        "{\"filesystem\":\"None\",\"network\":\"Allowlist\",\"exec\":\"None\"}\n",
    )
    .unwrap();
    let contract = work.join("contract.json");
    let archive = work.join("art.tar");

    let out = Command::new(lex_bin())
        .current_dir(&pkg)
        .args([
            "pkg",
            "publish",
            "--sign",
            SECRET,
            "--requires",
            grant.to_str().unwrap(),
            "--egress",
            "api.weather.example",
            "--contract-out",
            contract.to_str().unwrap(),
            "--archive-out",
            archive.to_str().unwrap(),
            "--no-upload",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (contract, archive)
}

fn write_keyring(work: &Path, keys: &[&str]) -> std::path::PathBuf {
    let path = work.join("keyring.json");
    let trusted = keys
        .iter()
        .map(|k| format!("\"{k}\""))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(&path, format!("{{\"trusted\":[{trusted}]}}\n")).unwrap();
    path
}

fn verify(contract: &Path, archive: &Path, keyring: Option<&Path>) -> std::process::Output {
    let mut args = vec![
        "pkg".to_string(),
        "verify".to_string(),
        "--archive".to_string(),
        archive.to_str().unwrap().to_string(),
        "--contract".to_string(),
        contract.to_str().unwrap().to_string(),
    ];
    if let Some(k) = keyring {
        args.push("--trusted-keys".to_string());
        args.push(k.to_str().unwrap().to_string());
    }
    Command::new(lex_bin()).args(&args).output().unwrap()
}

#[test]
fn publish_emits_a_contract_that_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, archive) = publish_signed(tmp.path());

    // The emitted contract binds the right artifact, grant, and signer.
    let signed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&contract).unwrap()).unwrap();
    assert_eq!(signed["contract"]["artifact"]["name"], "lex-weather");
    assert_eq!(signed["contract"]["artifact"]["version"], "1.2.0");
    assert_eq!(signed["contract"]["requires"]["network"], "Allowlist");
    assert_eq!(signed["contract"]["egress"][0], "api.weather.example");
    assert_eq!(signed["signer"], signer_pubkey());

    // With the publisher's key pinned, verification passes all three gates.
    let keyring = write_keyring(tmp.path(), &[&signer_pubkey()]);
    let out = verify(&contract, &archive, Some(&keyring));
    assert!(
        out.status.success(),
        "verify should pass: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("verified  lex-weather@1.2.0"));
    assert!(stdout.contains("signer_trust_checked=true"));
}

#[test]
fn verify_rejects_a_substituted_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, archive) = publish_signed(tmp.path());
    // Swap the archive bytes for something else: integrity gate must fire.
    std::fs::write(&archive, b"not the published bytes").unwrap();
    let keyring = write_keyring(tmp.path(), &[&signer_pubkey()]);
    let out = verify(&contract, &archive, Some(&keyring));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("integrity"));
}

#[test]
fn verify_rejects_a_tampered_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, archive) = publish_signed(tmp.path());
    // Edit a signed field (the required grant) after signing: signature breaks.
    let raw = std::fs::read_to_string(&contract).unwrap();
    // The emitted contract is pretty-printed: `"network": "Allowlist"`.
    let tampered = raw.replace("\"network\": \"Allowlist\"", "\"network\": \"Full\"");
    assert_ne!(raw, tampered, "the replace must actually change the contract");
    std::fs::write(&contract, tampered).unwrap();
    let keyring = write_keyring(tmp.path(), &[&signer_pubkey()]);
    let out = verify(&contract, &archive, Some(&keyring));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("authenticity"));
}

#[test]
fn publish_can_derive_the_grant_from_typed_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("lex.toml"),
        "[package]\nname = \"lex-weather\"\nversion = \"1.2.0\"\n",
    )
    .unwrap();
    // Declared effect rows: fs_read + host-scoped + bare net. No --requires.
    std::fs::write(
        pkg.join("src/main.lex"),
        "fn fetch() -> [net(\"api.weather.example\"), fs_read] Int { 0 }\nfn main() -> [net] Int { fetch() }\n",
    )
    .unwrap();

    let contract = tmp.path().join("contract.json");
    let archive = tmp.path().join("art.tar");
    let out = Command::new(lex_bin())
        .current_dir(&pkg)
        .args([
            "pkg",
            "publish",
            "--sign",
            SECRET,
            "--derive-grant",
            "--contract-out",
            contract.to_str().unwrap(),
            "--archive-out",
            archive.to_str().unwrap(),
            "--no-upload",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "derive publish failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The contract requires exactly the least authority the effects imply.
    let signed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&contract).unwrap()).unwrap();
    let c = &signed["contract"];
    assert_eq!(c["requires"]["filesystem"], "ReadOnly");
    assert_eq!(c["requires"]["network"], "Allowlist");
    assert_eq!(c["requires"]["exec"], "None");
    assert_eq!(c["egress"][0], "api.weather.example");

    // And it verifies (signature + content_hash) under the publisher's key.
    let keyring = write_keyring(tmp.path(), &[&signer_pubkey()]);
    let v = verify(&contract, &archive, Some(&keyring));
    assert!(
        v.status.success(),
        "derived contract should verify: {}",
        String::from_utf8_lossy(&v.stderr)
    );
}

#[test]
fn publish_rejects_requires_and_derive_together() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = tmp.path().join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("lex.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/main.lex"), "fn main() -> Int { 0 }\n").unwrap();
    let grant = tmp.path().join("grant.json");
    std::fs::write(&grant, "{\"filesystem\":\"None\",\"network\":\"None\",\"exec\":\"None\"}\n").unwrap();

    let out = Command::new(lex_bin())
        .current_dir(&pkg)
        .args([
            "pkg",
            "publish",
            "--sign",
            SECRET,
            "--derive-grant",
            "--requires",
            grant.to_str().unwrap(),
            "--no-upload",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("mutually exclusive"));
}

#[test]
fn verify_rejects_an_untrusted_signer() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, archive) = publish_signed(tmp.path());
    // A keyring that trusts some other key: authorization gate must fire.
    let other = lex_vcs::Keypair::from_secret_hex(&"aa".repeat(32))
        .unwrap()
        .public_hex();
    let keyring = write_keyring(tmp.path(), &[&other]);
    let out = verify(&contract, &archive, Some(&keyring));
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not in the trusted keyring"));
}
