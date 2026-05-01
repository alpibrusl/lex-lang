//! End-to-end tests for `lex ast-diff`. Builds two source files,
//! runs the diff, asserts on the structured output. The shape:
//!
//!   { added, removed, renamed, modified[{body_patches: [...]}] }
//!
//! Renames are detected by hashing the FnDecl with the name field
//! cleared — same body + same signature + different name = rename.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

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

/// Write `text` to a tempfile under `/tmp` and return its path.
fn tmp(name: &str, text: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lex_diff_test_{name}.lex"));
    let mut f = std::fs::File::create(&p).expect("create tmp");
    f.write_all(text.as_bytes()).expect("write tmp");
    p
}

#[test]
fn no_changes_reports_clean() {
    let a = tmp("identical_a", "fn id(x :: Int) -> Int { x }\n");
    let b = tmp("identical_b", "fn id(x :: Int) -> Int { x }\n");
    let (code, stdout, stderr) = run(&[
        "ast-diff", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("(no structural changes)"), "stdout: {stdout}");
}

#[test]
fn added_function_surfaces_in_added_list() {
    let a = tmp("a_only_one", "fn one() -> Int { 1 }\n");
    let b = tmp("b_two", "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let added = v["added"].as_array().expect("added array");
    assert_eq!(added.len(), 1);
    assert_eq!(added[0]["name"], "two");
    assert_eq!(v["removed"].as_array().unwrap().len(), 0);
    assert_eq!(v["modified"].as_array().unwrap().len(), 0);
}

#[test]
fn removed_function_surfaces_in_removed_list() {
    let a = tmp("a_two", "fn one() -> Int { 1 }\nfn two() -> Int { 2 }\n");
    let b = tmp("b_only_one", "fn one() -> Int { 1 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let removed = v["removed"].as_array().expect("removed array");
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0]["name"], "two");
}

#[test]
fn modified_body_with_same_signature_surfaces_with_body_patch() {
    let a = tmp("a_double", "fn doubled(n :: Int) -> Int { n * 2 }\n");
    let b = tmp("b_double", "fn doubled(n :: Int) -> Int { n + n }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let modified = v["modified"].as_array().expect("modified array");
    assert_eq!(modified.len(), 1);
    let m = &modified[0];
    assert_eq!(m["name"], "doubled");
    assert_eq!(m["signature_changed"], false);
    let patches = m["body_patches"].as_array().expect("body_patches");
    assert!(!patches.is_empty(), "expected at least one body patch");
}

#[test]
fn modified_signature_records_before_after() {
    let a = tmp("a_sig", "fn foo(n :: Int) -> Int { n }\n");
    let b = tmp("b_sig", "fn foo(n :: Int) -> [io] Int { n }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let modified = v["modified"].as_array().expect("modified array");
    assert_eq!(modified.len(), 1);
    let m = &modified[0];
    assert_eq!(m["signature_changed"], true);
    assert!(m["signature_before"].as_str().unwrap().contains("-> Int"));
    assert!(m["signature_after"].as_str().unwrap().contains("[io]"));
}

#[test]
fn rename_detected_when_body_matches_modulo_name() {
    // Identical body, same signature, different name → rename.
    let a = tmp("a_rename", "fn shouted(s :: Str) -> Str { s }\n");
    let b = tmp("b_rename", "fn say(s :: Str) -> Str { s }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let renamed = v["renamed"].as_array().expect("renamed array");
    assert_eq!(renamed.len(), 1);
    let r = &renamed[0];
    assert_eq!(r["from"], "shouted");
    assert_eq!(r["to"], "say");
    // Pure renames don't show up in added/removed.
    assert_eq!(v["added"].as_array().unwrap().len(), 0);
    assert_eq!(v["removed"].as_array().unwrap().len(), 0);
}

#[test]
fn different_bodies_with_different_names_are_add_remove_not_rename() {
    // Bodies differ → not a rename even though the signatures match.
    let a = tmp("a_add", "fn shouted(s :: Str) -> Str { s }\n");
    let b = tmp("b_add", "fn greet(name :: Str) -> Str { match name { _ => name } }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["renamed"].as_array().unwrap().len(), 0,
        "expected no false rename; got {v}");
    assert_eq!(v["added"].as_array().unwrap().len(), 1);
    assert_eq!(v["removed"].as_array().unwrap().len(), 1);
}

#[test]
fn text_output_is_human_readable() {
    let a = tmp("a_text",
        "fn doubled(n :: Int) -> Int { n * 2 }\nfn dropme() -> Int { 1 }\n");
    let b = tmp("b_text",
        "fn doubled(n :: Int) -> Int { n + n }\nfn newone() -> Int { 99 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("modified"), "stdout:\n{stdout}");
    assert!(stdout.contains("added")  || stdout.contains("+"), "stdout:\n{stdout}");
    assert!(stdout.contains("removed") || stdout.contains("-"), "stdout:\n{stdout}");
}

#[test]
fn no_body_flag_skips_body_patches() {
    let a = tmp("a_nobody", "fn doubled(n :: Int) -> Int { n * 2 }\n");
    let b = tmp("b_nobody", "fn doubled(n :: Int) -> Int { n + n }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", "--no-body",
        a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let m = &v["modified"][0];
    assert_eq!(m["body_patches"].as_array().unwrap().len(), 0,
        "expected --no-body to suppress body patches");
}

#[test]
fn parse_errors_surface_clearly() {
    let a = tmp("a_bad", "fn ok() -> Int { 1 }\n");
    let b = tmp("b_bad", "fn broken( - this is not valid lex\n");
    let (code, _stdout, stderr) = run(&[
        "ast-diff", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("parse"), "stderr: {stderr}");
}
