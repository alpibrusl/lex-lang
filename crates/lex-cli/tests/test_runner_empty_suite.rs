//! `lex test` on an empty (or missing) test directory must fail, not
//! silently pass. Before this fix, zero `test_*.lex` files found meant
//! `Ok(())` — exit 0 — the same "vacuously true" trap `task_spec.lex`
//! (lex-code) already guards against for a spec with zero criteria.
//! A caller gating on "the suite passes" (CI, or a coding agent's own
//! mechanical verify step) could not distinguish "everything passed"
//! from "nothing ran yet".

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-test-runner-empty-suite-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_test_in(cwd: &std::path::Path) -> (i32, String, String) {
    let out = Command::new(lex_bin())
        .arg("test")
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex test");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn missing_tests_dir_fails() {
    let dir = unique_dir("missing-dir");
    // No `tests/` directory created at all.

    let (code, _stdout, stderr) = run_test_in(&dir);
    assert_ne!(code, 0, "missing tests/ dir must not exit 0");
    assert!(
        stderr.contains("no test_*.lex files found"),
        "stderr should name the real cause, got: {stderr}"
    );
}

#[test]
fn empty_tests_dir_fails() {
    let dir = unique_dir("empty-dir");
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    // `tests/` exists but holds no `test_*.lex` file.

    let (code, _stdout, stderr) = run_test_in(&dir);
    assert_ne!(code, 0, "empty tests/ dir must not exit 0");
    assert!(
        stderr.contains("no test_*.lex files found"),
        "stderr should name the real cause, got: {stderr}"
    );
}

#[test]
fn nonempty_tests_dir_still_passes() {
    // Regression guard: the fix must not turn a genuinely passing
    // suite into a failure.
    let dir = unique_dir("nonempty-dir");
    let tests_dir = dir.join("tests");
    std::fs::create_dir_all(&tests_dir).unwrap();
    std::fs::write(tests_dir.join("test_ok.lex"), "fn run_all() -> Int { 0 }\n").unwrap();

    let (code, stdout, _stderr) = run_test_in(&dir);
    assert_eq!(code, 0, "a real passing suite must still exit 0");
    assert!(stdout.contains("1 passed, 0 failed"), "got: {stdout}");
}
