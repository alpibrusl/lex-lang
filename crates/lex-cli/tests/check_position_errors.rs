//! `lex check` emits source positions for type errors (#306 slice 1).
//!
//! LLM-driven repair flows rely on `file:line:col` to locate the
//! offending function; this test pins the contract that the JSON
//! envelope carries `position { file, line, col }` for every error.

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
    let dir = std::env::temp_dir().join(format!("lex-check-pos-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn type_error_carries_source_position() {
    // The `bad` function on line 3 has a return-type mismatch:
    // the body produces a Str but the signature declares Int.
    let src = "\
fn ok_fn(x :: Int) -> Int { x }

fn bad(x :: Int) -> Int { \"oops\" }
";
    let path = write_to_tempfile("posbad.lex", src);
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 2, "must exit nonzero on type error: {stdout}");

    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|_| panic!("envelope parses as JSON: {stdout}"));
    let errors = envelope.pointer("/data/errors").expect("errors array present");
    let arr = errors.as_array().expect("errors is an array");
    assert!(!arr.is_empty(), "at least one error: {stdout}");

    // Find the type-mismatch error for `bad` and check its position.
    let first = &arr[0];
    let pos = first
        .get("position")
        .unwrap_or_else(|| panic!("position present: {first}"));
    assert_eq!(
        pos.get("line").and_then(|v| v.as_u64()),
        Some(3),
        "fn `bad` is on line 3: {first}"
    );
    assert_eq!(
        pos.get("col").and_then(|v| v.as_u64()),
        Some(1),
        "fn `bad` starts at col 1: {first}"
    );
    let file = pos.get("file").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        file.ends_with("posbad.lex"),
        "file path should match the input: {file}"
    );
}

#[test]
fn ok_program_does_not_break_on_position_plumbing() {
    // Regression: enriching the check pipeline with positions must
    // not break the ok-path. The pure-program envelope still emits
    // ok:true with no errors and exit 0.
    let path = write_to_tempfile(
        "posgood.lex",
        "fn add(x :: Int, y :: Int) -> Int { x + y }\n",
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 0, "ok program: {stdout}");
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("envelope parses");
    let ok = envelope.pointer("/data/ok").and_then(|v| v.as_bool());
    assert_eq!(ok, Some(true), "ok: true expected: {stdout}");
}

#[test]
fn position_resolves_to_correct_function_when_multiple_fail() {
    // Two functions, both broken. Each error must report the line
    // of its own function — not of the first or last only.
    let src = "\
fn first_broken(x :: Int) -> Str { x }


fn second_ok(x :: Int) -> Int { x }


fn third_broken(s :: Str) -> Int { s }
";
    let path = write_to_tempfile("posmulti.lex", src);
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 2);

    let envelope: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = envelope
        .pointer("/data/errors")
        .and_then(|v| v.as_array())
        .expect("errors array");
    assert_eq!(arr.len(), 2, "two distinct fn errors: {stdout}");

    let lines: Vec<u64> = arr
        .iter()
        .map(|e| {
            e.get("position")
                .and_then(|p| p.get("line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        })
        .collect();
    assert!(
        lines.contains(&1) && lines.contains(&7),
        "errors must report lines of their own fns (1 and 7), got {:?}",
        lines
    );
}
