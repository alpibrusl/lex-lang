//! End-to-end tests for `lex audit`. Exercises the four filters
//! (effect / calls / uses-host / kind) against the existing example
//! files in the workspace.

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

fn examples() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples")
}

#[test]
fn audit_no_filter_lists_every_fn_and_summarizes_by_effect() {
    let (code, stdout, stderr) = run(&["audit", examples()]);
    assert_eq!(code, 0, "stderr:\n{stderr}");
    // Summary header must appear.
    assert!(stdout.contains("SUMMARY:"), "stdout:\n{stdout}");
    // Some known fns from the example set.
    assert!(stdout.contains("factorial"), "stdout missing factorial");
    assert!(stdout.contains("handle"),    "stdout missing handle");
    // Effect rollup buckets at least pure + [net].
    assert!(stdout.contains("pure"),  "stdout: {stdout}");
    assert!(stdout.contains("[net]"), "stdout: {stdout}");
}

#[test]
fn audit_filter_by_effect_net_finds_network_touchpoints() {
    let (code, stdout, _stderr) = run(&[
        "audit", "--effect", "net", "--no-summary", examples(),
    ]);
    assert_eq!(code, 0);
    // Every reported hit must mention [net] in its rendered signature.
    for line in stdout.lines().filter(|l| l.contains("fn ")) {
        assert!(line.contains("[net") || line.contains("net]")
                || line.contains("[net]"),
            "non-network fn surfaced under --effect net: {line}");
    }
    // The weather route is the canonical hit.
    assert!(stdout.contains("route_weather"), "stdout:\n{stdout}");
}

#[test]
fn audit_filter_by_calls_finds_concrete_callsite() {
    let (code, stdout, _stderr) = run(&[
        "audit", "--calls", "net.post", "--no-summary", examples(),
    ]);
    assert_eq!(code, 0);
    // inbox_app's handle_important is the one fn that actually calls net.post.
    assert!(stdout.contains("handle_important"), "stdout:\n{stdout}");
}

#[test]
fn audit_filter_by_uses_host_finds_string_literals() {
    let (code, stdout, _stderr) = run(&[
        "audit", "--uses-host", "wttr.in", "--no-summary", examples(),
    ]);
    assert_eq!(code, 0);
    // gateway_app's route_weather hardcodes wttr.in as the upstream.
    assert!(stdout.contains("route_weather"), "stdout:\n{stdout}");
}

#[test]
fn audit_filter_by_kind_finds_fns_using_that_node() {
    let (code, stdout, _stderr) = run(&[
        "audit", "--kind", "Match", "--no-summary", examples(),
    ]);
    assert_eq!(code, 0);
    // factorial uses match on Int — confirms the kind filter walks
    // top-level expressions.
    assert!(stdout.contains("factorial"), "stdout:\n{stdout}");
}

#[test]
fn audit_json_output_is_parseable() {
    let (code, stdout, _stderr) = run(&[
        "audit", "--effect", "net", "--json", examples(),
    ]);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(&stdout)
        .expect("audit --json must emit valid JSON");
    assert!(v.get("summary").is_some(), "missing summary: {v}");
    assert!(v.get("hits").is_some(),    "missing hits: {v}");
    let hits = v["hits"].as_array().expect("hits is array");
    assert!(!hits.is_empty(), "expected at least one [net] hit");
    // Each hit carries the structured fields the agent eval loop
    // expects.
    let first = &hits[0];
    for k in &["file", "name", "effects", "signature", "matched"] {
        assert!(first.get(k).is_some(), "missing field {k} in {first}");
    }
}

#[test]
fn audit_empty_filters_combined_match_acts_as_or() {
    // Two filters; a fn matching either must surface. inbox_app's
    // handle_important matches both --effect=net AND --calls=net.post,
    // so it should appear once with both reasons listed.
    let (code, stdout, _stderr) = run(&[
        "audit", "--effect", "net", "--calls", "net.post",
        "--no-summary", examples(),
    ]);
    assert_eq!(code, 0);
    // The 'matched' tag must include both reasons.
    let lines: Vec<&str> = stdout.lines()
        .filter(|l| l.contains("handle_important")).collect();
    assert!(!lines.is_empty(), "no handle_important hit; stdout:\n{stdout}");
    let line = lines[0];
    assert!(line.contains("effect=") && line.contains("calls="),
        "match reasons missing: {line}");
}

#[test]
fn audit_unknown_path_errors() {
    let (code, _stdout, stderr) = run(&["audit", "/no/such/path"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("stat") || stderr.contains("No such"),
        "stderr:\n{stderr}");
}
