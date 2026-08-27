//! `lex test` must fail a file that REPORTS failures, not only one that raises.
//!
//! Discarding `run_all`'s return value silently passed every suite that returned
//! a failure count — which is most of them, because `std.test.assert_*` returns a
//! `Result` rather than aborting. A green run then meant "the file loaded"
//! (lex-lang#757).

use std::path::{Path, PathBuf};
use std::process::Command;

fn unique_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "lex-verdict-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("tests")).unwrap();
    d
}

fn write_test(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join("tests").join(name), body).unwrap();
}

fn run(dir: &Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_lex"))
        .args(["test", "tests"])
        .current_dir(dir)
        .output()
        .expect("run lex test");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

#[test]
fn a_nonzero_failure_count_fails_the_file() {
    let dir = unique_dir("count");
    write_test(&dir, "test_counted.lex", "fn run_all() -> Int {\n  3\n}\n");
    let (code, stdout) = run(&dir);
    assert_ne!(code, 0, "a reported failure count must fail the run:\n{stdout}");
    assert!(
        stdout.contains("3 failing assertion"),
        "the count should be reported:\n{stdout}"
    );
}

#[test]
fn a_zero_failure_count_passes() {
    let dir = unique_dir("zero");
    write_test(&dir, "test_clean.lex", "fn run_all() -> Int {\n  0\n}\n");
    let (code, stdout) = run(&dir);
    assert_eq!(code, 0, "zero failures is a pass:\n{stdout}");
}

#[test]
fn an_err_in_a_returned_list_fails_the_file() {
    let dir = unique_dir("list");
    write_test(
        &dir,
        "test_listed.lex",
        "fn run_all() -> List[Result[Unit, Str]] {\n  [Ok(()), Err(\"the tenant header was not stamped\"), Ok(())]\n}\n",
    );
    let (code, stdout) = run(&dir);
    assert_ne!(code, 0, "an Err in the results must fail the run:\n{stdout}");
    assert!(
        stdout.contains("tenant header"),
        "the failure message should be surfaced, not just counted:\n{stdout}"
    );
}

#[test]
fn a_list_of_oks_passes() {
    let dir = unique_dir("oks");
    write_test(
        &dir,
        "test_ok.lex",
        "fn run_all() -> List[Result[Unit, Str]] {\n  [Ok(()), Ok(())]\n}\n",
    );
    let (code, stdout) = run(&dir);
    assert_eq!(code, 0, "all-Ok results is a pass:\n{stdout}");
}

#[test]
fn the_documented_unit_convention_still_passes() {
    let dir = unique_dir("unit");
    write_test(&dir, "test_unit.lex", "fn run_all() -> Unit {\n  ()\n}\n");
    let (code, stdout) = run(&dir);
    assert_eq!(code, 0, "`fn run_all() -> ()` is the documented shape:\n{stdout}");
}
