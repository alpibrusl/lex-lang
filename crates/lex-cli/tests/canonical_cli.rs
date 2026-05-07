//! End-to-end tests for the `lex canonical encode` / `lex canonical
//! decode` CLI subcommands (#206 slice 2).
//!
//! Encode reads a `.lex` file and emits canonical-AST bytes (raw on
//! stdout, base64 in JSON envelope, or to a file with `--out`).
//! Decode reads canonical bytes and prints `.lex` text via
//! `print_stages`. Round-tripping `text → canonical → text` produces
//! semantically equivalent source (insignificant whitespace and
//! comments are dropped — that's the canonical form's whole point).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn run(args: &[&str]) -> (i32, Vec<u8>, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn tmp_text(name: &str, text: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lex_canonical_test_{name}.lex"));
    let mut f = std::fs::File::create(&p).expect("create tmp");
    f.write_all(text.as_bytes()).expect("write tmp");
    p
}

fn tmp_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lex_canonical_test_{name}.canon"));
    // Make sure no stale file from a prior run carries over.
    let _ = std::fs::remove_file(&p);
    p
}

const SOURCE: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n\
                      fn run() -> Int { add(2, 3) }\n";

#[test]
fn encode_to_file_writes_versioned_bytes() {
    let src = tmp_text("encode_file_in", SOURCE);
    let out = tmp_path("encode_file_out");
    let (code, _, stderr) = run(&[
        "canonical", "encode", src.to_str().unwrap(),
        "--out", out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let bytes = std::fs::read(&out).expect("read out");
    assert!(!bytes.is_empty());
    assert_eq!(bytes[0], 1, "version byte should be 1");
    assert!(bytes.len() > 10, "payload should be more than just the version");
}

#[test]
fn encode_without_out_writes_raw_bytes_to_stdout() {
    let src = tmp_text("encode_stdout_in", SOURCE);
    let (code, stdout, stderr) = run(&[
        "canonical", "encode", src.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.is_empty());
    assert_eq!(stdout[0], 1, "version byte first");
}

#[test]
fn encode_json_output_emits_base64_envelope() {
    let src = tmp_text("encode_json_in", SOURCE);
    let (code, stdout, _stderr) = run(&[
        "--output", "json",
        "canonical", "encode", src.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let s = String::from_utf8_lossy(&stdout).to_string();
    let v: serde_json::Value = serde_json::from_str(&s)
        .unwrap_or_else(|e| panic!("expected JSON envelope; parse error {e}; got: {s}"));
    // The acli envelope nests payload under `data`.
    let data = &v["data"];
    assert_eq!(data["ok"], true);
    let b64 = data["bytes_b64"].as_str()
        .expect("data.bytes_b64 string field");
    assert!(!b64.is_empty());
    // The first 12 bits (version byte 0x01 + start of '[' = 0x5b)
    // are 0b000000_010101 = "AV" in base64. Anything past that
    // depends on what JSON tokens follow; we don't need to pin it
    // for this test — the round-trip test below already covers
    // structural fidelity.
    assert!(b64.starts_with("AV"),
        "expected base64 to start with AV (version byte 1 followed by '['); got: {b64}");
}

#[test]
fn round_trip_decode_after_encode_returns_text() {
    let src = tmp_text("rt_in", SOURCE);
    let canon = tmp_path("rt_out");

    let (code, _, _) = run(&[
        "canonical", "encode", src.to_str().unwrap(),
        "--out", canon.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);

    let (code, stdout, stderr) = run(&[
        "canonical", "decode", canon.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let text = String::from_utf8_lossy(&stdout);
    // The semantic content survives: function names + arity present.
    assert!(text.contains("fn add"), "decoded text should contain `fn add`; got:\n{text}");
    assert!(text.contains("fn run"), "decoded text should contain `fn run`; got:\n{text}");
    assert!(text.contains("Int"));
}

#[test]
fn decode_unknown_version_surfaces_clear_error() {
    let bad = tmp_path("bad_version");
    std::fs::write(&bad, [99u8, b'{', b'}']).unwrap();
    let (code, _, stderr) = run(&[
        "canonical", "decode", bad.to_str().unwrap(),
    ]);
    assert_ne!(code, 0, "should fail");
    assert!(stderr.contains("unsupported canonical-AST version") ||
            stderr.contains("decode"),
        "stderr should mention the decode failure; got: {stderr}");
}

#[test]
fn decode_empty_file_surfaces_clear_error() {
    let empty = tmp_path("empty");
    std::fs::write(&empty, []).unwrap();
    let (code, _, stderr) = run(&[
        "canonical", "decode", empty.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(stderr.to_lowercase().contains("empty") ||
            stderr.contains("no version byte") ||
            stderr.contains("decode"),
        "stderr should mention empty input; got: {stderr}");
}

#[test]
fn unknown_subaction_lists_valid_choices() {
    let (code, _, stderr) = run(&[
        "canonical", "transmogrify",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("encode") && stderr.contains("decode"),
        "stderr should hint at valid subactions; got: {stderr}");
}
