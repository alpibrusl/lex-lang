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
fn effect_changes_surface_added_and_removed() {
    // before: pure `safe`, [net] `risky`.
    // after:  [io] `safe`,  [net, fs_read] `risky`.
    // Expected: safe gains [io]; risky gains [fs_read].
    let a = tmp("a_eff",
        "fn safe(x :: Int) -> Int { x }\n\
         fn risky() -> [net] Int { 0 }\n");
    let b = tmp("b_eff",
        "fn safe(x :: Int) -> [io] Int { x }\n\
         fn risky() -> [net, fs_read] Int { 0 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let modified = v["modified"].as_array().expect("modified array");
    assert_eq!(modified.len(), 2);

    let by_name: std::collections::BTreeMap<&str, &serde_json::Value> =
        modified.iter()
            .map(|m| (m["name"].as_str().unwrap(), m))
            .collect();

    let safe = by_name["safe"];
    let added: Vec<&str> = safe["effect_changes"]["added"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(added, vec!["io"]);
    assert!(safe["effect_changes"]["removed"].as_array().unwrap().is_empty());

    let risky = by_name["risky"];
    let added: Vec<&str> = risky["effect_changes"]["added"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(added, vec!["fs_read"]);
}

#[test]
fn effect_dropped_shows_in_removed() {
    // [io, net] → [net]: io is dropped.
    let a = tmp("a_drop", "fn f() -> [io, net] Int { 0 }\n");
    let b = tmp("b_drop", "fn f() -> [net] Int { 0 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let m = &v["modified"][0];
    let removed: Vec<&str> = m["effect_changes"]["removed"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(removed, vec!["io"]);
    assert!(m["effect_changes"]["added"].as_array().unwrap().is_empty());
}

#[test]
fn effect_arg_change_surfaces_as_paired_add_remove() {
    // fs_read("/tmp") → fs_read("/etc"): semantically distinct
    // scopes; show as one removed + one added so reviewers see the
    // path change explicitly.
    let a = tmp("a_arg",
        "fn f(p :: Str) -> [fs_read(\"/tmp\")] Str { p }\n");
    let b = tmp("b_arg",
        "fn f(p :: Str) -> [fs_read(\"/etc\")] Str { p }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let m = &v["modified"][0];
    let added: Vec<&str> = m["effect_changes"]["added"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    let removed: Vec<&str> = m["effect_changes"]["removed"].as_array().unwrap()
        .iter().map(|x| x.as_str().unwrap()).collect();
    assert_eq!(added.len(), 1);
    assert_eq!(removed.len(), 1);
    assert!(added[0].contains("/etc"), "added: {added:?}");
    assert!(removed[0].contains("/tmp"), "removed: {removed:?}");
}

#[test]
fn effects_unchanged_yields_empty_added_removed() {
    // Only the body changed; effects are identical.
    let a = tmp("a_body_only", "fn f() -> [io] Int { 1 }\n");
    let b = tmp("b_body_only", "fn f() -> [io] Int { 2 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", "--json", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let m = &v["modified"][0];
    assert!(m["effect_changes"]["added"].as_array().unwrap().is_empty());
    assert!(m["effect_changes"]["removed"].as_array().unwrap().is_empty());
}

#[test]
fn text_mode_flags_effect_gain() {
    let a = tmp("a_text_eff", "fn f() -> Int { 1 }\n");
    let b = tmp("b_text_eff", "fn f() -> [io] Int { 1 }\n");
    let (code, stdout, _) = run(&[
        "ast-diff", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("effects gained"), "stdout: {stdout}");
    assert!(stdout.contains("io"), "stdout: {stdout}");
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
