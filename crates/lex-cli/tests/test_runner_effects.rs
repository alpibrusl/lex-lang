//! `lex test`'s permissive policy must include every effect declared
//! in the stdlib catalog so test files that reach `[sql]` (or any
//! other stdlib effect) don't fail at the runner-level policy gate
//! (#399). The `--allow-effects` flag is the escape hatch for vendor-
//! extension effects we don't ship and for tightening a test's
//! allow-list to verify effect-shape contracts.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-test-effects-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &std::path::Path, name: &str, src: &str) {
    let p = dir.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, src).unwrap();
}

fn run_test_in(cwd: &std::path::Path, extra_args: &[&str]) -> (i32, String, String) {
    let out = Command::new(lex_bin())
        .arg("test")
        .args(extra_args)
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

fn write_sql_test(dir: &std::path::Path) {
    write(
        dir,
        "lex.toml",
        "[package]\nname = \"sql_repro\"\nversion = \"0.1.0\"\n",
    );
    // `sql.open(":memory:")` carries [sql, fs_write] in the stdlib
    // signature (the SQLite driver creates a file on disk-backed
    // opens; the in-memory case still types as fs_write for symmetry).
    write(
        dir,
        "tests/test_with_sql.lex",
        r#"import "std.sql" as sql

fn run_all() -> [sql, fs_write] Int {
  match sql.open(":memory:") {
    Ok(_db) => 0,
    Err(_e) => 1,
  }
}
"#,
    );
}

#[test]
fn permissive_policy_allows_sql_effect_in_test_run() {
    // Headline contract of #399: a test file that hits `[sql]`
    // succeeds under the runner's default permissive policy.
    let dir = unique_dir("sql-permissive");
    write_sql_test(&dir);

    let (code, stdout, stderr) = run_test_in(&dir, &[]);
    assert_eq!(
        code, 0,
        "expected default permissive `lex test` to allow `[sql]`.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed, 0 failed"),
        "expected `1 passed, 0 failed`, got:\n{stdout}"
    );
}

#[test]
fn explicit_allow_effects_flag_overrides_permissive() {
    // --allow-effects sql,fs_write replaces the permissive default
    // with a tight allow-list. Test still passes because both
    // effects are listed.
    let dir = unique_dir("sql-explicit-ok");
    write_sql_test(&dir);

    let (code, stdout, _) =
        run_test_in(&dir, &["--allow-effects", "sql,fs_write"]);
    assert_eq!(code, 0, "expected pass with explicit allow-list: {stdout}");
    assert!(stdout.contains("1 passed"));
}

#[test]
fn explicit_allow_effects_flag_rejects_missing_effect() {
    // --allow-effects io alone is *not* enough to cover [sql,
    // fs_write], so the runner must reject. This proves the flag
    // actually narrows the policy — it doesn't just augment
    // permissive.
    let dir = unique_dir("sql-explicit-fail");
    write_sql_test(&dir);

    let (code, stdout, _) = run_test_in(&dir, &["--allow-effects", "io"]);
    assert_ne!(code, 0, "expected non-zero exit; got 0 with stdout:\n{stdout}");
    assert!(
        stdout.contains("effect `sql` not in --allow-effects"),
        "expected policy violation on `sql`, got:\n{stdout}"
    );
}
