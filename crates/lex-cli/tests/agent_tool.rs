//! End-to-end tests for the `lex agent-tool` subcommand. Each test
//! spawns the built binary with `--body` (no Anthropic API call) and
//! asserts on the exit code + stdout/stderr.

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

#[test]
fn benign_pure_body_runs_and_returns_string() {
    // No effects — a fully pure tool that just builds a greeting.
    let (code, stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--input", "world",
        "--body", "str.concat(\"hello, \", input)",
    ]);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert!(stdout.trim() == "hello, world", "stdout: {stdout:?}");
}

#[test]
fn malicious_body_using_io_read_is_rejected_at_typecheck() {
    // tool only allowed `[net]`; body tries io.read. The type checker
    // should reject before any code runs.
    let (code, stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "net",
        "--quiet",
        "--input", "x",
        "--body",
        "match io.read(\"/etc/passwd\") { Ok(s) => s, Err(e) => e }",
    ]);
    assert_ne!(code, 0, "expected non-zero exit; stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(code, 2, "expected exit code 2 (type-check rejection)");
    assert!(stderr.contains("TYPE-CHECK REJECTED"), "stderr:\n{stderr}");
    assert!(stderr.contains("effect `io`"), "stderr:\n{stderr}");
    assert!(!stdout.contains("root:"), "io.read output leaked to stdout: {stdout:?}");
}

#[test]
fn malicious_body_writing_files_is_rejected_at_typecheck() {
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "net",
        "--quiet",
        "--input", "x",
        "--body",
        "match io.write(\"/tmp/leak\", input) { Ok(_) => \"wrote\", Err(e) => e }",
    ]);
    assert_eq!(code, 2, "expected type-check rejection; stderr:\n{stderr}");
    assert!(stderr.contains("effect `io`"), "stderr:\n{stderr}");
}

#[test]
fn body_is_recovered_from_a_full_fn_block_with_fences() {
    // Models often wrap output in ```lex ... ``` and inline a `fn tool(...)`
    // declaration; strip_code_fences should peel that down to the inner body.
    let body = "```lex\nfn tool(input :: Str) -> Str {\n  str.concat(\"got: \", input)\n}\n```";
    let (code, stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--input", "ping",
        "--body", body,
    ]);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout.trim(), "got: ping");
}

#[test]
fn fs_write_effect_rejected_when_only_net_allowed() {
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "net",
        "--quiet",
        "--input", "x",
        "--body",
        // io.print is just an [io] effect — not in [net]
        "{ let _ := io.print(input); \"done\" }",
    ]);
    // Either the parser rejects the let-with-underscore or the type
    // checker rejects the io effect — both are non-zero exits.
    assert_ne!(code, 0, "expected rejection; stderr:\n{stderr}");
}

#[test]
fn step_limit_aborts_runaway_compute() {
    // 10_000-element fold ≈ 120k ops; capped at 5_000 steps the VM
    // aborts well before it finishes. Without this guard, an LLM-emitted
    // `list.fold(list.range(0, BIG), ...)` would hang the host.
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--max-steps", "5000",
        "--input", "x",
        "--body",
        "int.to_str(list.fold(list.range(0, 10000), 0, \
         fn (a :: Int, b :: Int) -> Int { a + b }))",
    ]);
    assert_eq!(code, 4, "expected step-limit exit (4); stderr:\n{stderr}");
    assert!(stderr.contains("STEP-LIMIT"), "stderr:\n{stderr}");
}

#[test]
fn benign_compute_runs_within_default_step_limit() {
    // Default --max-steps is generous (1M); a 100-element fold finishes.
    let (code, stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--input", "x",
        "--body",
        "int.to_str(list.fold(list.range(0, 100), 0, \
         fn (a :: Int, b :: Int) -> Int { a + b }))",
    ]);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    // sum 0..100 = 4950
    assert_eq!(stdout.trim(), "4950");
}

#[test]
fn examples_pass_when_body_matches_expected() {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push("lex_examples_pass.json");
    let mut f = std::fs::File::create(&tmp).expect("create tmp");
    f.write_all(br#"[{"input":"21","expected":"42"},{"input":"100","expected":"200"}]"#)
        .expect("write tmp");

    let (code, stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--examples", tmp.to_str().unwrap(),
        "--input", "5",
        "--body",
        "match str.to_int(input) { Some(n) => int.to_str(n * 2), None => \"err\" }",
    ]);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    // The single-shot --input run prints 10 (5 * 2) to stdout.
    assert_eq!(stdout.trim(), "10");
}

#[test]
fn examples_reject_wrong_behavior_with_exit_5() {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push("lex_examples_fail.json");
    let mut f = std::fs::File::create(&tmp).expect("create tmp");
    f.write_all(br#"[{"input":"21","expected":"42"},{"input":"100","expected":"200"}]"#)
        .expect("write tmp");

    // Body triples instead of doubling — well-typed but wrong behavior.
    // Type-check passes; --examples catches it before live use.
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--examples", tmp.to_str().unwrap(),
        "--input", "5",
        "--body",
        "match str.to_int(input) { Some(n) => int.to_str(n * 3), None => \"err\" }",
    ]);
    assert_eq!(code, 5, "expected exit 5; stderr:\n{stderr}");
    assert!(stderr.contains("EXAMPLES FAILED"), "stderr:\n{stderr}");
    assert!(stderr.contains("expected=\"42\""), "stderr:\n{stderr}");
    assert!(stderr.contains("got     =\"63\""), "stderr:\n{stderr}");
}

#[test]
fn examples_missing_file_errors_clearly() {
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--examples", "/no/such/path/examples.json",
        "--input", "x",
        "--body", "input",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("read examples file"), "stderr:\n{stderr}");
}

#[test]
fn spec_proved_against_correct_body() {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push("lex_spec_ok.spec");
    let mut f = std::fs::File::create(&tmp).expect("create spec");
    f.write_all(br#"spec is_ok { forall s :: Str: tool(s) == "ok" }"#)
        .expect("write spec");

    let (code, stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--spec", tmp.to_str().unwrap(),
        "--input", "x",
        "--body", r#""ok""#,
    ]);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    assert_eq!(stdout.trim(), "ok");
}

#[test]
fn spec_counterexample_aborts_with_exit_5() {
    use std::io::Write;
    let mut tmp = std::env::temp_dir();
    tmp.push("lex_spec_fail.spec");
    let mut f = std::fs::File::create(&tmp).expect("create spec");
    f.write_all(br#"spec is_ok { forall s :: Str: tool(s) == "ok" }"#)
        .expect("write spec");

    // Body always returns "oops" — spec asserts "ok"; randomized prover
    // immediately finds a counterexample (any input string).
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--spec", tmp.to_str().unwrap(),
        "--input", "x",
        "--body", r#""oops""#,
    ]);
    assert_eq!(code, 5, "expected exit 5; stderr:\n{stderr}");
    assert!(stderr.contains("SPEC COUNTEREXAMPLE"), "stderr:\n{stderr}");
}

#[test]
fn spec_missing_file_errors_clearly() {
    let (code, _stdout, stderr) = run(&[
        "agent-tool",
        "--allow-effects", "",
        "--quiet",
        "--spec", "/no/such/path/some.spec",
        "--input", "x",
        "--body", r#""ok""#,
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("read spec file"), "stderr:\n{stderr}");
}
