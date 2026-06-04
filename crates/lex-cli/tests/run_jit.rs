//! `lex run --jit <file> <fn> [args]` must produce the same result
//! as `lex run` without the flag — on a function the JIT eligibility
//! predicate accepts (pure int arith), the JIT compiles + runs it;
//! otherwise the interpreter handles it transparently.
//!
//! The test programs below are deliberately narrow shapes the MVP
//! JIT can actually digest. Cross-validating against the plain
//! `lex run` output guarantees we haven't silently miscompiled —
//! exactly the same correctness check the JIT's unit tests do, but
//! driven through the public CLI surface so the wire-up is
//! end-to-end verified.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

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

fn write_tempfile(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lex-run-jit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn jit_pure_arith_matches_interpreter() {
    // The simplest possible eligible function: int arithmetic with
    // no records, no closures, no effects. Both interpreter and JIT
    // must produce the same result.
    let path = write_tempfile(
        "arith.lex",
        r#"
fn poly(a :: Int, b :: Int, c :: Int) -> Int {
  a * b + c
}
"#,
    );
    let (code_a, out_a, err_a) = run(&[
        "run",
        path.to_str().unwrap(),
        "poly",
        "7",
        "11",
        "13",
    ]);
    assert_eq!(code_a, 0, "interp run failed: {err_a}");

    let (code_b, out_b, err_b) = run(&[
        "run",
        "--jit",
        path.to_str().unwrap(),
        "poly",
        "7",
        "11",
        "13",
    ]);
    assert_eq!(code_b, 0, "jit run failed: {err_b}");

    assert_eq!(
        out_a.trim(),
        out_b.trim(),
        "interp and --jit produced different output:\ninterp: {out_a:?}\njit:    {out_b:?}"
    );
    // The result should also be 7*11 + 13 = 90 — confirms we didn't
    // both fail to compute correctly in the same way.
    assert!(
        out_a.trim().contains("90"),
        "expected output to contain 90, got {out_a:?}"
    );
}

#[test]
fn jit_falls_through_for_ineligible_program() {
    // A program with records / strings — completely outside the MVP
    // JIT's op set. The `--jit` flag should silently fall through to
    // the interpreter and produce identical output.
    let path = write_tempfile(
        "greet.lex",
        r#"
type Person = { name :: Str, age :: Int }

fn greet(p :: Person) -> Str {
  p.name
}
"#,
    );
    let person = r#"{"name":"Ada","age":36}"#;
    let (code_a, out_a, err_a) = run(&[
        "run", path.to_str().unwrap(), "greet", person,
    ]);
    assert_eq!(code_a, 0, "interp run failed: {err_a}");

    let (code_b, out_b, err_b) = run(&[
        "run", "--jit", path.to_str().unwrap(), "greet", person,
    ]);
    assert_eq!(code_b, 0, "jit run failed: {err_b}");

    assert_eq!(out_a.trim(), out_b.trim(),
        "interp vs --jit diverged on ineligible program:\ninterp: {out_a:?}\njit:    {out_b:?}");
    assert!(out_a.contains("Ada"), "expected greet to return Ada, got {out_a:?}");
}

#[test]
fn jit_refuses_explicit_max_steps() {
    // Security guard (cursor[bot] medium-severity review on #608):
    // JIT'd code runs native loops that don't bump `Vm::steps`, so
    // `--max-steps` would be silently bypassed. The CLI rejects the
    // combination rather than degrade the documented DoS guard.
    let path = write_tempfile(
        "arith2.lex",
        r#"
fn double(n :: Int) -> Int {
  n + n
}
"#,
    );
    let (code, _out, err) = run(&[
        "run",
        "--jit",
        "--max-steps",
        "1000000",
        path.to_str().unwrap(),
        "double",
        "21",
    ]);
    assert_ne!(code, 0, "expected --jit + --max-steps to fail, succeeded: {err}");
    assert!(
        err.contains("mutually exclusive"),
        "expected error to explain the incompatibility, got: {err:?}"
    );
}

#[test]
fn jit_composes_with_unrelated_flags() {
    // Sanity: `--jit` alone works (no `--max-steps`). This is the
    // common-path replacement for the prior test that combined
    // `--jit --max-steps`, which is now rejected.
    let path = write_tempfile(
        "arith3.lex",
        r#"
fn double(n :: Int) -> Int {
  n + n
}
"#,
    );
    let (code, out, err) = run(&[
        "run", "--jit", path.to_str().unwrap(), "double", "21",
    ]);
    assert_eq!(code, 0, "--jit alone failed: {err}");
    assert!(out.trim().contains("42"), "expected 42, got {out:?}");
}
