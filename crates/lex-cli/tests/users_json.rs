//! `<store>/users.json` actor enforcement (lex-tea v3d, #172).
//! When the file exists, `lex stage <pin|defer|block|unblock>`
//! refuses unknown actors. When absent, behaviour matches v3a–v3c.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, src: &std::path::Path) -> serde_json::Value {
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "publish: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn first_stage_id(publish_out: &serde_json::Value) -> String {
    publish_out.pointer("/data/ops/0/kind/stage_id")
        .and_then(|v| v.as_str())
        .expect("stage_id")
        .to_string()
}

fn write_users_json(store: &std::path::Path, names: &[&str]) {
    let users: Vec<serde_json::Value> = names.iter()
        .map(|n| serde_json::json!({"name": n, "role": "human"}))
        .collect();
    let body = serde_json::json!({"users": users});
    std::fs::write(store.join("users.json"), serde_json::to_vec_pretty(&body).unwrap()).unwrap();
}

#[test]
fn pin_with_known_actor_succeeds() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);
    write_users_json(store.path(), &["alice", "bob"]);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "spec wrong",
            "--actor", "alice",
            "--store", store.path().to_str().unwrap(),
        ])
        .env_remove("LEX_TEA_USER")
        .output().unwrap();
    assert!(out.status.success(), "pin: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn pin_with_unknown_actor_refused() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);
    write_users_json(store.path(), &["alice", "bob"]);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "spec wrong",
            "--actor", "eve",
            "--store", store.path().to_str().unwrap(),
        ])
        .env_remove("LEX_TEA_USER")
        .output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("eve"), "stderr should name the offender: {stderr}");
    assert!(stderr.contains("users.json"), "stderr should mention users.json: {stderr}");
}

#[test]
fn defer_block_unblock_all_validate() {
    // Same gate applies to every triage verb.
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);
    write_users_json(store.path(), &["alice"]);

    for verb in ["defer", "block", "unblock"] {
        let out = Command::new(lex_bin())
            .args([
                "stage", verb, &stage_id,
                "--reason", "x",
                "--actor", "eve",
                "--store", store.path().to_str().unwrap(),
            ])
            .env_remove("LEX_TEA_USER")
            .output().unwrap();
        assert!(!out.status.success(), "{verb} should refuse unknown actor");
    }
}

#[test]
fn lex_tea_user_env_must_also_be_listed() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);
    write_users_json(store.path(), &["alice"]);

    // LEX_TEA_USER=eve, but eve is not in users.json → refuse.
    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "x",
            "--store", store.path().to_str().unwrap(),
        ])
        .env("LEX_TEA_USER", "eve")
        .output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn no_users_json_keeps_v3c_behaviour() {
    // Without the file, any --actor is accepted (existing flow).
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n :: Int) -> Int { 1 }\n").unwrap();
    let pub_out = publish(store.path(), &src);
    let stage_id = first_stage_id(&pub_out);

    let out = Command::new(lex_bin())
        .args([
            "stage", "pin", &stage_id,
            "--reason", "x",
            "--actor", "anyone",
            "--store", store.path().to_str().unwrap(),
        ])
        .env_remove("LEX_TEA_USER")
        .output().unwrap();
    assert!(out.status.success(),
        "without users.json any actor works: {}", String::from_utf8_lossy(&out.stderr));
}
