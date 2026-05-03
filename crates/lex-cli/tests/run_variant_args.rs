//! `lex run` accepts `{"$variant": ..., "args": [...]}` for variant
//! arguments (closes #93).

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

fn write_to_tempfile(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lex-run-variant-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn lex_run_decodes_tag_only_variant_arg() {
    let path = write_to_tempfile(
        "color.lex",
        r#"type Color = Red | Green | Blue

fn name(c :: Color) -> Str {
  match c {
    Red   => "red",
    Green => "green",
    Blue  => "blue",
  }
}
"#,
    );
    let (code, stdout) = run(&[
        "run",
        path.to_str().unwrap(),
        "name",
        r#"{"$variant":"Red","args":[]}"#,
    ]);
    assert_eq!(code, 0, "exit code; stdout: {stdout}");
    assert!(stdout.contains("red"), "stdout: {stdout}");
}

#[test]
fn lex_run_decodes_variant_with_payload() {
    let path = write_to_tempfile(
        "opt.lex",
        r#"fn unwrap_or(o :: Option[Int], default :: Int) -> Int {
  match o {
    Some(n) => n,
    None    => default,
  }
}
"#,
    );
    let (code, stdout) = run(&[
        "run",
        path.to_str().unwrap(),
        "unwrap_or",
        r#"{"$variant":"Some","args":[42]}"#,
        "0",
    ]);
    assert_eq!(code, 0, "exit code; stdout: {stdout}");
    assert!(stdout.trim() == "42", "stdout: {stdout}");
}

#[test]
fn lex_run_decodes_nested_variant_in_record() {
    let path = write_to_tempfile(
        "nested.lex",
        r#"type Status = Healthy | Sick
type Report = { score :: Int, status :: Status }

fn label(r :: Report) -> Str {
  match r.status {
    Healthy => "ok",
    Sick    => "nope",
  }
}
"#,
    );
    let (code, stdout) = run(&[
        "run",
        path.to_str().unwrap(),
        "label",
        r#"{"score":7,"status":{"$variant":"Healthy","args":[]}}"#,
    ]);
    assert_eq!(code, 0, "exit code; stdout: {stdout}");
    assert!(stdout.contains("ok"), "stdout: {stdout}");
}

#[test]
fn lex_run_round_trips_variant_output() {
    // The function returns a variant; it should serialize with the
    // same convention so it can be piped back as input.
    let path = write_to_tempfile(
        "echo_variant.lex",
        r#"type Color = Red | Green | Blue
fn echo(c :: Color) -> Color { c }
"#,
    );
    let (code, stdout) = run(&[
        "run",
        path.to_str().unwrap(),
        "echo",
        r#"{"$variant":"Green","args":[]}"#,
    ]);
    assert_eq!(code, 0, "exit code; stdout: {stdout}");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("expected JSON output, got {stdout:?}: {e}"));
    assert_eq!(parsed["$variant"], "Green");
    assert!(parsed["args"].is_array());
}
