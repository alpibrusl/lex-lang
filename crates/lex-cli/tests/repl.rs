//! Black-box tests for `lex repl`. Drive the binary with stdin
//! input, assert on stdout. Each block of input ends with `.quit`.

use std::io::Write;
use std::process::{Command, Stdio};

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn run_repl(input: &str) -> String {
    let mut child = Command::new(lex_bin())
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lex repl");
    child.stdin.as_mut().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().expect("wait");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn repl_evaluates_arithmetic() {
    let out = run_repl("1 + 2 * 3\n.quit\n");
    assert!(out.contains("=> 7"), "stdout: {out}");
}

#[test]
fn repl_returns_string_literal_unquoted_via_json_wrapper() {
    let out = run_repl("\"hello, world\"\n.quit\n");
    // json.stringify wraps Str in quotes; the REPL strips the outer
    // Str layer and prints the JSON, so the user sees the quotes.
    assert!(out.contains("\"hello, world\""), "stdout: {out}");
}

#[test]
fn repl_evaluates_list_expression() {
    let out = run_repl("[1, 2, 3]\n.quit\n");
    assert!(out.contains("[1,2,3]"), "stdout: {out}");
}

#[test]
fn repl_keeps_definitions_across_inputs() {
    let out = run_repl("fn double(x :: Int) -> Int { x * 2 }\ndouble(21)\n.quit\n");
    assert!(out.contains("=> 42"), "stdout: {out}");
}

#[test]
fn repl_reports_type_errors_inline() {
    let out = run_repl("\"a\" + 1\n.quit\n");
    assert!(out.contains("error:"), "stdout: {out}");
}

#[test]
fn repl_reset_clears_session() {
    let out = run_repl(
        "fn keep(n :: Int) -> Int { n + 1 }\n\
         .reset\n\
         keep(0)\n\
         .quit\n");
    assert!(out.contains("(session cleared)"), "stdout: {out}");
    assert!(out.contains("error:"), "stdout: {out}");
}
