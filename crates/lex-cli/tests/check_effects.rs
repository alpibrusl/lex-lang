//! `lex check` surfaces the effects a program will need at run time
//! (closes #81). The pure-program path stays silent so we don't bury
//! the existing `ok` line in noise.

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
    let dir = std::env::temp_dir().join(format!("lex-check-effects-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn pure_program_check_is_silent_on_effects() {
    let path = write_to_tempfile(
        "pure.lex",
        "fn add(x :: Int, y :: Int) -> Int { x + y }\n",
    );
    let (code, stdout) = run(&["check", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("ok"), "stdout: {stdout}");
    assert!(
        !stdout.contains("required effects"),
        "pure program shouldn't mention required effects: {stdout}"
    );
}

#[test]
fn io_program_check_surfaces_io() {
    let path = write_to_tempfile(
        "echo.lex",
        r#"import "std.io" as io
fn echo(s :: Str) -> [io] Nil { io.print(s) }
"#,
    );
    let (code, stdout) = run(&["check", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("ok"));
    assert!(
        stdout.contains("required effects: io"),
        "expected text hint, got: {stdout}"
    );
    assert!(
        stdout.contains("--allow-effects io"),
        "expected suggested run command, got: {stdout}"
    );
}

#[test]
fn json_check_includes_required_effects_array() {
    let path = write_to_tempfile(
        "echo_json.lex",
        r#"import "std.io" as io
fn echo(s :: Str) -> [io] Nil { io.print(s) }
"#,
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    let data = &parsed["data"];
    assert_eq!(data["ok"], true);
    let kinds = data["required_effects"].as_array().expect("array");
    assert!(
        kinds.iter().any(|v| v == "io"),
        "expected `io` in required_effects, got: {kinds:?}"
    );
}

#[test]
fn pure_program_in_json_has_empty_effects_array() {
    let path = write_to_tempfile(
        "pure_json.lex",
        "fn add(x :: Int, y :: Int) -> Int { x + y }\n",
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    let kinds = parsed["data"]["required_effects"].as_array().expect("array");
    assert!(kinds.is_empty(), "pure program should report no effects");
}

#[test]
fn multiple_effects_in_signature_are_all_surfaced() {
    let path = write_to_tempfile(
        "multi.lex",
        r#"import "std.io" as io
import "std.net" as net
fn fetch_and_print(url :: Str) -> [io, net] Result[Nil, Str] {
  match net.get(url) {
    Ok(body) => Ok(io.print(body)),
    Err(e) => Err(e),
  }
}
"#,
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    let kinds: Vec<&str> = parsed["data"]["required_effects"]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // We sort with BTreeSet so the order is alphabetical.
    assert!(kinds.contains(&"io"), "got {kinds:?}");
    assert!(kinds.contains(&"net"), "got {kinds:?}");
}

#[test]
fn fs_read_path_argument_surfaces_separately() {
    // The path argument on `fs_read` is a runtime-policy concern (it
    // gates `--allow-fs-read PATH`); the type system tracks the
    // declaration but doesn't bind it to a specific stdlib call here,
    // so a bare declaration with a pure body is the cleanest fixture.
    let path = write_to_tempfile(
        "read_data.lex",
        "fn dummy() -> [fs_read(\"/data\")] Int { 42 }\n",
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("json output");
    let kinds: Vec<&str> = parsed["data"]["required_effects"]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"fs_read"), "got kinds: {kinds:?}");
    let paths: Vec<&str> = parsed["data"]["required_fs_read"]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"/data"),
        "expected /data in required_fs_read, got: {paths:?}"
    );
}
