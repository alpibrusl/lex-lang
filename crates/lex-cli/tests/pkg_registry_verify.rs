//! Conformance for `lex pkg install`'s registry provenance gate: a registry
//! dependency is verified against its published signed contract (authenticity +
//! content hash + optional signer trust) before it is installed.
//!
//! A `tiny_http` mock registry serves the `lex pkg publish`-emitted contract and
//! archive at `/v1/pkg/{name}/{version}/{contract,archive}`, closing the
//! publish → verify loop end-to-end across the same contract format `lex-os
//! capsule install` consumes.

use std::path::Path;
use std::process::Command;
use std::thread;

fn lex_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

const SECRET: &str = "1122334455667788990011223344556677889900112233445566778899001122";

fn signer_pubkey() -> String {
    lex_vcs::Keypair::from_secret_hex(SECRET).unwrap().public_hex()
}

/// Publish a signed package; returns its (contract bytes, archive bytes).
fn publish(tmp: &Path) -> (Vec<u8>, Vec<u8>) {
    let pkg = tmp.join("pkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("lex.toml"),
        "[package]\nname = \"dep\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(pkg.join("src/lib.lex"), "fn helper(x :: Int) -> Int { x + 1 }\n").unwrap();
    let grant = tmp.join("grant.json");
    std::fs::write(
        &grant,
        "{\"filesystem\":\"None\",\"network\":\"None\",\"exec\":\"None\"}\n",
    )
    .unwrap();
    let contract = tmp.join("c.json");
    let archive = tmp.join("a.tar");
    let out = Command::new(lex_bin())
        .current_dir(&pkg)
        .args([
            "pkg",
            "publish",
            "--sign",
            SECRET,
            "--requires",
            grant.to_str().unwrap(),
            "--contract-out",
            contract.to_str().unwrap(),
            "--archive-out",
            archive.to_str().unwrap(),
            "--no-upload",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
    (std::fs::read(&contract).unwrap(), std::fs::read(&archive).unwrap())
}

/// Spawn a mock registry serving the contract and archive; returns its base URL.
fn spawn_registry(contract: Vec<u8>, archive: Vec<u8>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => unreachable!("expected IP listener"),
    };
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            if url.ends_with("/contract") {
                let _ = req.respond(tiny_http::Response::from_data(contract.clone()));
            } else if url.ends_with("/archive") {
                let _ = req.respond(tiny_http::Response::from_data(archive.clone()));
            } else {
                let _ = req.respond(tiny_http::Response::empty(404));
            }
        }
    });
    format!("http://{addr}")
}

/// Run `lex pkg install` in a fresh consumer project depending on `dep` from
/// `registry`, with an isolated package cache.
fn install(tmp: &Path, registry: &str, keyring: Option<&Path>) -> std::process::Output {
    let consumer = tmp.join("app");
    std::fs::create_dir_all(&consumer).unwrap();
    std::fs::write(
        consumer.join("lex.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\ndep = {{ registry = \"{registry}\", version = \"0.1.0\" }}\n"
        ),
    )
    .unwrap();
    let mut args = vec!["pkg".to_string(), "install".to_string()];
    if let Some(k) = keyring {
        args.push("--trusted-keys".to_string());
        args.push(k.to_str().unwrap().to_string());
    }
    Command::new(lex_bin())
        .current_dir(&consumer)
        .env("LEX_PACKAGES_DIR", tmp.join("cache"))
        .args(&args)
        .output()
        .unwrap()
}

fn write_keyring(tmp: &Path, key: &str) -> std::path::PathBuf {
    let p = tmp.join("keyring.json");
    std::fs::write(&p, format!("{{\"trusted\":[\"{key}\"]}}\n")).unwrap();
    p
}

#[test]
fn install_verifies_a_registry_dep_against_its_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, archive) = publish(tmp.path());
    let registry = spawn_registry(contract, archive);
    let keyring = write_keyring(tmp.path(), &signer_pubkey());

    let out = install(tmp.path(), &registry, Some(&keyring));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "install should succeed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("contract verified"), "stdout={stdout}");
    assert!(stdout.contains("signer trusted"), "stdout={stdout}");
}

#[test]
fn install_refuses_a_tampered_registry_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, _archive) = publish(tmp.path());
    // Serve a valid contract but substituted archive bytes.
    let registry = spawn_registry(contract, b"not the published bytes".to_vec());

    let out = install(tmp.path(), &registry, None);
    assert!(!out.status.success(), "tampered archive must be refused");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("integrity"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn install_refuses_an_untrusted_registry_signer() {
    let tmp = tempfile::tempdir().unwrap();
    let (contract, archive) = publish(tmp.path());
    let registry = spawn_registry(contract, archive);
    // A keyring that trusts some other key.
    let other = lex_vcs::Keypair::from_secret_hex(&"aa".repeat(32))
        .unwrap()
        .public_hex();
    let keyring = write_keyring(tmp.path(), &other);

    let out = install(tmp.path(), &registry, Some(&keyring));
    assert!(!out.status.success(), "untrusted signer must be refused");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not in the trusted keyring"),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
