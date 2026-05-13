//! `lex check` runs `examples { ... }` cases through the VM and emits a
//! structured `example-mismatch` error when the body's actual output
//! disagrees with the declared `expected` value (#369 slice 2).
//!
//! Slice 1 (PR #370) shipped the type-level checks. This suite covers the
//! behavioral checks added in slice 2.

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
    let dir = std::env::temp_dir().join(format!("lex-example-eval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn passing_example_keeps_check_green() {
    // The body genuinely produces `5` for `id(5)`, so `lex check` is happy.
    let path = write_to_tempfile(
        "ok.lex",
        "fn id(x :: Int) -> Int\n  examples { id(5) => 5 }\n{ x }\n",
    );
    let (code, stdout) = run(&["check", path.to_str().unwrap()]);
    assert_eq!(code, 0, "expected 0 (ok), got {code}: {stdout}");
    assert!(stdout.contains("ok"), "stdout should say 'ok': {stdout}");
}

#[test]
fn behavioral_mismatch_fires_example_mismatch() {
    // Body returns x, but the example claims id(5) => 999. The type-level
    // checks pass (both 5 and 999 are Int), so this case is the load-
    // bearing one for slice 2 — it must fail at `lex check` time.
    let path = write_to_tempfile(
        "stale.lex",
        "fn id(x :: Int) -> Int\n  examples { id(5) => 999 }\n{ x }\n",
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 2, "behavioral mismatch must exit nonzero: {stdout}");
    let envelope: serde_json::Value = serde_json::from_str(&stdout)
        .expect("output should be JSON when --output json is set");
    let errors = envelope
        .pointer("/data/errors")
        .and_then(|v| v.as_array())
        .expect("envelope must carry errors array at /data/errors");
    assert_eq!(errors.len(), 1, "exactly one mismatch expected: {stdout}");
    let err = &errors[0];
    assert_eq!(err.get("kind").and_then(|v| v.as_str()), Some("example_mismatch"));
    assert_eq!(err.get("rule_tag").and_then(|v| v.as_str()), Some("example-mismatch"));
    assert_eq!(err.get("fn_name").and_then(|v| v.as_str()), Some("id"));
    assert_eq!(err.get("case_index").and_then(|v| v.as_i64()), Some(0));
    assert_eq!(err.get("expected").and_then(|v| v.as_str()), Some("999"));
    assert_eq!(err.get("got").and_then(|v| v.as_str()), Some("5"));
}

#[test]
fn recursive_function_with_passing_examples() {
    // factorial is the canonical showcase. The body actually produces the
    // declared values, so this should pass after slice 2.
    let path = write_to_tempfile(
        "factorial.lex",
        "fn factorial(n :: Int) -> Int\n  \
            examples {\n    \
                factorial(0) => 1,\n    \
                factorial(1) => 1,\n    \
                factorial(5) => 120\n  \
            }\n\
         {\n  \
           match n {\n    \
             0 => 1,\n    \
             _ => n * factorial(n - 1),\n  \
           }\n\
         }\n",
    );
    let (code, stdout) = run(&["check", path.to_str().unwrap()]);
    assert_eq!(code, 0, "factorial examples must pass: {stdout}");
}

#[test]
fn recursive_function_with_one_stale_case() {
    // Three cases declared; only the middle one regressed. The error
    // envelope must identify the offending case_index.
    let path = write_to_tempfile(
        "factorial_stale.lex",
        "fn factorial(n :: Int) -> Int\n  \
            examples {\n    \
                factorial(0) => 1,\n    \
                factorial(1) => 7,\n    \
                factorial(5) => 120\n  \
            }\n\
         {\n  \
           match n {\n    \
             0 => 1,\n    \
             _ => n * factorial(n - 1),\n  \
           }\n\
         }\n",
    );
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 2, "stale middle example must surface: {stdout}");
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let errors = envelope
        .pointer("/data/errors")
        .and_then(|v| v.as_array())
        .expect("errors array at /data/errors");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].get("case_index").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(errors[0].get("expected").and_then(|v| v.as_str()), Some("7"));
    assert_eq!(errors[0].get("got").and_then(|v| v.as_str()), Some("1"));
}

#[test]
fn variant_result_compares_structurally() {
    // The expected value is a constructor call; the body produces the same
    // shape. Demonstrates that PartialEq on Value covers variants.
    let path = write_to_tempfile(
        "wrap.lex",
        "fn wrap(x :: Int) -> Option[Int]\n  \
            examples { wrap(3) => Some(3) }\n\
         { Some(x) }\n",
    );
    let (code, stdout) = run(&["check", path.to_str().unwrap()]);
    assert_eq!(code, 0, "variant equality must work: {stdout}");
}
