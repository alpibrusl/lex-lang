//! `lex test` must resolve aliased path imports the same way `lex check`
//! does — by routing the test file through the multi-file loader rather
//! than parsing it in isolation (#395).
//!
//! Before the fix, `import "../src/lib" as lib` in a test file produced
//! `unknown_identifier "lib"` because the runner used the bare parser
//! and never expanded / mangled imports.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-test-runner-imports-{}-{}-{tag}",
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
fn aliased_path_import_resolves_in_test_runner() {
    // The exact reproducer from issue #395: a test file uses
    // `import "../src/lib" as lib` plus a stdlib import, and exercises
    // both aliases. Pre-fix this failed at type-check with
    // `unknown_identifier "lib"`.
    let dir = unique_dir("aliased-path");

    write(
        &dir,
        "lex.toml",
        "[package]\nname = \"alias_repro\"\nversion = \"0.1.0\"\n",
    );
    write(
        &dir,
        "src/lib.lex",
        "fn greet(name :: Str) -> Str { \"hello, \" }\n",
    );
    write(
        &dir,
        "tests/test_lib.lex",
        r#"import "../src/lib" as lib
import "std.list"   as list

fn one_test() -> Result[Unit, Str] {
  let r := lib.greet("world")
  if r == "hello, " { Ok(()) } else { Err("nope") }
}

fn suite() -> List[Result[Unit, Str]] { [one_test()] }
fn run_all() -> Int {
  list.fold(suite(), 0, fn (n :: Int, r :: Result[Unit, Str]) -> Int {
    match r { Ok(_) => n, Err(_) => n + 1 }
  })
}
"#,
    );

    let (code, stdout, stderr) = run_test_in(&dir);
    assert_eq!(
        code, 0,
        "expected `lex test` to pass after #395 fix.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed, 0 failed"),
        "expected summary `1 passed, 0 failed`, got:\n{stdout}"
    );
}

#[test]
fn helper_referenced_via_alias_from_test_file_runs() {
    // The test file imports a sibling helper file and calls into it via
    // the alias. The previous code path skipped the loader and so the
    // alias never expanded — failing at type-check.
    let dir = unique_dir("helper-alias");

    write(
        &dir,
        "src/math.lex",
        "fn double(x :: Int) -> Int { x + x }\n",
    );
    write(
        &dir,
        "tests/test_math.lex",
        r#"import "../src/math" as m

fn run_all() -> Int { m.double(21) }
"#,
    );

    let (code, stdout, stderr) = run_test_in(&dir);
    assert_eq!(
        code, 0,
        "expected `lex test` to pass.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
