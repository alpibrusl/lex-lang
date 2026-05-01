//! End-to-end tests for `lex ast-merge`. Builds three source files
//! (base, ours, theirs), runs the merge, asserts on the structured
//! output. Covers the four conflict kinds (modify-modify,
//! modify-delete, delete-modify, add-add) and the clean-merge paths.

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

fn tmp(name: &str, text: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lex_merge_test_{name}.lex"));
    let mut f = std::fs::File::create(&p).expect("create tmp");
    f.write_all(text.as_bytes()).expect("write tmp");
    p
}

#[test]
fn identical_inputs_merge_cleanly_with_zero_changes() {
    let body = "fn id(x :: Int) -> Int { x }\n";
    let b = tmp("identical_base", body);
    let o = tmp("identical_ours", body);
    let t = tmp("identical_theirs", body);
    let (code, stdout, stderr) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["summary"]["clean"], 1);
    assert_eq!(v["summary"]["conflicts"], 0);
    // Source unchanged on both sides → "base" provenance.
    assert_eq!(v["merged"][0]["from"], "base");
}

#[test]
fn ours_modified_only_takes_ours() {
    let b = tmp("oo_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("oo_ours",   "fn f() -> Int { 2 }\n");
    let t = tmp("oo_theirs", "fn f() -> Int { 1 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["summary"]["conflicts"], 0);
    assert_eq!(v["merged"][0]["from"], "ours");
}

#[test]
fn theirs_modified_only_takes_theirs() {
    let b = tmp("tt_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("tt_ours",   "fn f() -> Int { 1 }\n");
    let t = tmp("tt_theirs", "fn f() -> Int { 2 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["merged"][0]["from"], "theirs");
}

#[test]
fn both_made_identical_change_takes_either() {
    let b = tmp("be_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("be_ours",   "fn f() -> Int { 2 }\n");
    let t = tmp("be_theirs", "fn f() -> Int { 2 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["summary"]["conflicts"], 0);
    assert_eq!(v["merged"][0]["from"], "both");
}

#[test]
fn modify_modify_conflict_emits_structured_json() {
    let b = tmp("mm_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("mm_ours",   "fn f() -> Int { 2 }\n");
    let t = tmp("mm_theirs", "fn f() -> Int { 3 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "expected exit 2 on conflict");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["summary"]["conflicts"], 1);
    let c = &v["conflicts"][0];
    assert_eq!(c["kind"], "modify-modify");
    assert_eq!(c["name"], "f");
    assert!(c["base"].as_str().unwrap().contains("1"));
    assert!(c["ours"].as_str().unwrap().contains("2"));
    assert!(c["theirs"].as_str().unwrap().contains("3"));
}

#[test]
fn modify_delete_conflict_when_ours_modifies_theirs_deletes() {
    let b = tmp("md_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("md_ours",   "fn f() -> Int { 2 }\n");
    let t = tmp("md_theirs", "");  // theirs deleted f
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let c = &v["conflicts"][0];
    assert_eq!(c["kind"], "modify-delete");
    assert!(c["base"].is_string());
    assert!(c["ours"].is_string());
    assert!(c["theirs"].is_null());
}

#[test]
fn delete_modify_conflict_when_ours_deletes_theirs_modifies() {
    let b = tmp("dm_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("dm_ours",   "");  // ours deleted f
    let t = tmp("dm_theirs", "fn f() -> Int { 2 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let c = &v["conflicts"][0];
    assert_eq!(c["kind"], "delete-modify");
    assert!(c["ours"].is_null());
    assert!(c["theirs"].is_string());
}

#[test]
fn add_add_conflict_when_both_add_same_name_with_different_bodies() {
    let b = tmp("aa_base",   "");
    let o = tmp("aa_ours",   "fn helper() -> Int { 1 }\n");
    let t = tmp("aa_theirs", "fn helper() -> Int { 2 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    let c = &v["conflicts"][0];
    assert_eq!(c["kind"], "add-add");
    assert!(c["base"].is_null());
    assert!(c["ours"].is_string());
    assert!(c["theirs"].is_string());
}

#[test]
fn add_add_with_identical_bodies_is_clean() {
    let body = "fn helper() -> Int { 1 }\n";
    let b = tmp("aa_clean_base", "");
    let o = tmp("aa_clean_ours", body);
    let t = tmp("aa_clean_theirs", body);
    let (code, stdout, _) = run(&[
        "ast-merge", "--json",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(v["summary"]["conflicts"], 0);
    assert_eq!(v["merged"][0]["from"], "added-both");
}

#[test]
fn output_writes_merged_source_when_clean() {
    let b = tmp("out_base",   "fn a() -> Int { 1 }\n");
    let o = tmp("out_ours",   "fn a() -> Int { 2 }\n");
    let t = tmp("out_theirs", "fn a() -> Int { 1 }\nfn b() -> Int { 99 }\n");
    let mut out_path = std::env::temp_dir();
    out_path.push("lex_merge_test_out_merged.lex");
    let _ = std::fs::remove_file(&out_path);

    let (code, _stdout, stderr) = run(&[
        "ast-merge", "--output", out_path.to_str().unwrap(),
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let merged = std::fs::read_to_string(&out_path).expect("merged file written");
    // ours' change to a() should be present (took ours).
    assert!(merged.contains("fn a("), "merged: {merged}");
    // theirs' added b() should be present.
    assert!(merged.contains("fn b("), "merged: {merged}");
}

#[test]
fn output_refuses_to_write_when_conflicts_present() {
    let b = tmp("ref_base",   "fn f() -> Int { 1 }\n");
    let o = tmp("ref_ours",   "fn f() -> Int { 2 }\n");
    let t = tmp("ref_theirs", "fn f() -> Int { 3 }\n");
    let mut out_path = std::env::temp_dir();
    out_path.push("lex_merge_test_should_not_exist.lex");
    let _ = std::fs::remove_file(&out_path);

    let (code, _stdout, stderr) = run(&[
        "ast-merge", "--output", out_path.to_str().unwrap(),
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("conflict"), "stderr: {stderr}");
    assert!(!out_path.exists(), "output file should not exist on conflict");
}

#[test]
fn text_output_lists_conflict_kinds() {
    let b = tmp("text_base",   "fn f() -> Int { 1 }\nfn g() -> Int { 1 }\n");
    let o = tmp("text_ours",   "fn f() -> Int { 2 }\nfn g() -> Int { 2 }\n");
    let t = tmp("text_theirs", "fn f() -> Int { 3 }\nfn g() -> Int { 1 }\n");
    let (code, stdout, _) = run(&[
        "ast-merge",
        b.to_str().unwrap(), o.to_str().unwrap(), t.to_str().unwrap(),
    ]);
    assert_eq!(code, 2);
    assert!(stdout.contains("modify-modify"), "stdout:\n{stdout}");
    assert!(stdout.contains("conflict"), "stdout:\n{stdout}");
}
