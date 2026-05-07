//! `lex run --from-store STAGE_ID` (#227 follow-up). Loads a stage's
//! canonical AST out of the store rather than from a file, optionally
//! enforcing Ed25519 signature policy via `--require-signed` /
//! `--trusted-key`.
//!
//! Tests publish a small fn into a fresh tempdir store (signed or
//! unsigned), then drive `lex run --from-store ...` and check the
//! return value plus the verification outcomes.

use std::process::{Command, Stdio};
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn run_with_env(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(lex_bin());
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in env { cmd.env(k, v); }
    let out = cmd.output().expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn run(args: &[&str]) -> (i32, String, String) {
    run_with_env(args, &[])
}

const SOURCE: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";

/// Pull the first add_function stage_id out of `lex publish` JSON.
fn first_stage_id(publish_json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(publish_json).expect("parse publish JSON");
    let ops = v.pointer("/data/ops")
        .or_else(|| v.get("ops"))
        .and_then(|x| x.as_array())
        .expect("ops array");
    for op in ops {
        if let Some(stg) = op.pointer("/kind/stage_id").and_then(|x| x.as_str()) {
            return stg.to_string();
        }
    }
    panic!("no stage_id in publish output: {publish_json}");
}

fn fresh_keypair() -> (String, String) {
    let (_, out, _) = run(&["--output", "json", "keygen"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    (
        v.pointer("/data/public_key").and_then(|x| x.as_str()).unwrap().to_string(),
        v.pointer("/data/secret_key").and_then(|x| x.as_str()).unwrap().to_string(),
    )
}

/// Publish SOURCE into `store_path`, optionally signing with `sk`.
/// Returns the StageId of the published function.
fn publish_into(store_path: &std::path::Path, sk: Option<&str>) -> String {
    let src = store_path.join("a.lex");
    std::fs::write(&src, SOURCE).unwrap();
    let mut args: Vec<&str> = vec![
        "--output", "json", "publish",
        "--store", store_path.to_str().unwrap(),
        "--activate",
    ];
    if let Some(sk) = sk {
        args.push("--signing-key");
        args.push(sk);
    }
    args.push(src.to_str().unwrap());
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "publish failed: {stderr}");
    first_stage_id(&stdout)
}

// ---- happy path -----------------------------------------------------

#[test]
fn run_from_store_calls_unsigned_stage() {
    let store = tempdir().unwrap();
    let stage_id = publish_into(store.path(), None);

    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, stdout, stderr) = run_with_env(&[
        "--output", "json", "run",
        "--from-store", &stage_id,
        "add", "2", "3",
    ], &env);
    assert_eq!(code, 0, "stderr: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let result = v.pointer("/data/result")
        .or_else(|| v.get("result"))
        .expect("result");
    // Lex Ints come back as strings in the JSON envelope (Value::Int
    // serialises as { "kind": "Int", "value": 5 } via value_to_json).
    let answer = result.pointer("/value").and_then(|x| x.as_i64())
        .or_else(|| result.as_i64())
        .expect("integer result");
    assert_eq!(answer, 5);
}

#[test]
fn run_from_store_calls_signed_stage_with_require_signed() {
    let store = tempdir().unwrap();
    let (_, sk) = fresh_keypair();
    let stage_id = publish_into(store.path(), Some(&sk));

    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, _, stderr) = run_with_env(&[
        "--output", "json", "run",
        "--from-store", &stage_id,
        "--require-signed",
        "add", "2", "3",
    ], &env);
    assert_eq!(code, 0, "stderr: {stderr}");
}

#[test]
fn run_from_store_with_trusted_key_accepts_matching_signer() {
    let store = tempdir().unwrap();
    let (pk, sk) = fresh_keypair();
    let stage_id = publish_into(store.path(), Some(&sk));

    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, _, stderr) = run_with_env(&[
        "--output", "json", "run",
        "--from-store", &stage_id,
        "--trusted-key", &pk,
        "add", "10", "20",
    ], &env);
    assert_eq!(code, 0, "stderr: {stderr}");
}

// ---- rejection paths ------------------------------------------------

#[test]
fn run_from_store_rejects_unsigned_under_require_signed() {
    let store = tempdir().unwrap();
    let stage_id = publish_into(store.path(), None);

    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, _, stderr) = run_with_env(&[
        "run",
        "--from-store", &stage_id,
        "--require-signed",
        "add", "1", "2",
    ], &env);
    assert_ne!(code, 0, "should refuse unsigned stage under --require-signed");
    assert!(stderr.contains("not signed") || stderr.contains("require"),
        "stderr should mention the missing signature; got: {stderr}");
}

#[test]
fn run_from_store_rejects_wrong_trusted_key() {
    let store = tempdir().unwrap();
    let (_pk_a, sk_a) = fresh_keypair();
    let (pk_b, _sk_b) = fresh_keypair();
    let stage_id = publish_into(store.path(), Some(&sk_a));

    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, _, stderr) = run_with_env(&[
        "run",
        "--from-store", &stage_id,
        "--trusted-key", &pk_b,
        "add", "1", "2",
    ], &env);
    assert_ne!(code, 0);
    assert!(stderr.contains("trusted") || stderr.contains("not by"),
        "stderr should explain the trust mismatch; got: {stderr}");
}

#[test]
fn run_from_store_unknown_stage_id_errors_clearly() {
    let store = tempdir().unwrap();
    // Force the store dir to exist but without any stages.
    std::fs::create_dir_all(store.path().join("stages")).unwrap();
    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, _, stderr) = run_with_env(&[
        "run",
        "--from-store", "deadbeef".repeat(8).as_str(),
        "add", "1", "2",
    ], &env);
    assert_ne!(code, 0);
    assert!(
        stderr.to_lowercase().contains("metadata")
            || stderr.to_lowercase().contains("unknown")
            || stderr.to_lowercase().contains("stage"),
        "stderr should mention the missing stage; got: {stderr}");
}

#[test]
fn run_from_store_missing_function_arg_errors() {
    let store = tempdir().unwrap();
    let stage_id = publish_into(store.path(), None);

    let env = [("LEX_STORE", store.path().to_str().unwrap())];
    let (code, _, stderr) = run_with_env(&[
        "run",
        "--from-store", &stage_id,
    ], &env);
    assert_ne!(code, 0);
    assert!(stderr.to_lowercase().contains("usage") || stderr.contains("--from-store"),
        "stderr should hint at usage; got: {stderr}");
}
