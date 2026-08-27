//! `lex fmt` must never delete a comment.
//!
//! The formatter prints from an AST that carries comments only on items, so a
//! comment inside a function body has nowhere to live and used to disappear
//! silently (lex-lang#755). Preserving it needs trivia on statements; until then
//! the file is left unchanged rather than quietly losing documentation.

use std::path::PathBuf;
use std::process::Command;

fn unique_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lex-fmtc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_fmt(dir: &PathBuf, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_lex"))
        .arg("fmt")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run lex fmt");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

const IN_BODY: &str = "fn f(x :: Int) -> Int {\n  # why this is not obvious\n  # and a second line\n  let y := x + 1\n  y\n}\n";

#[test]
fn a_comment_in_a_function_body_survives_formatting() {
    let dir = unique_dir("body");
    let file = dir.join("a.lex");
    std::fs::write(&file, IN_BODY).unwrap();

    let (_code, _stdout, stderr) = run_fmt(&dir, &["a.lex"]);
    let after = std::fs::read_to_string(&file).unwrap();

    assert!(
        after.contains("# why this is not obvious") && after.contains("# and a second line"),
        "formatting must not delete comments; file is now:\n{after}"
    );
    assert!(
        stderr.contains("would delete") && stderr.contains("755"),
        "the skip should be explained, not silent:\n{stderr}"
    );
}

#[test]
fn check_reports_a_file_it_cannot_format_without_loss() {
    let dir = unique_dir("check");
    std::fs::write(dir.join("a.lex"), IN_BODY).unwrap();

    let (code, _stdout, stderr) = run_fmt(&dir, &["--check", "a.lex"]);
    assert_ne!(code, 0, "--check must not report such a file as formatted");
    assert!(
        stderr.contains("would delete"),
        "--check should say why:\n{stderr}"
    );
}

#[test]
fn a_file_with_no_body_comments_still_formats() {
    let dir = unique_dir("plain");
    let file = dir.join("a.lex");
    // Deliberately mis-indented so the formatter has something to do.
    std::fs::write(&file, "# a leading comment on the item\nfn f(x :: Int) -> Int {\n      x + 1\n}\n").unwrap();

    let (code, _stdout, _stderr) = run_fmt(&dir, &["a.lex"]);
    let after = std::fs::read_to_string(&file).unwrap();

    assert_eq!(code, 0);
    assert!(
        after.contains("# a leading comment on the item"),
        "item comments were already preserved and must stay so:\n{after}"
    );
}
