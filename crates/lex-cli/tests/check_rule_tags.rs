//! `lex check` emits a stable `rule_tag` + plain-language
//! `rule_explanation` for every type error (#306 slice 2). LLM
//! repair prompts that reference the rule_tag get measurably
//! better repair attempts because the model can cross-reference
//! the rule across many prior examples.

use std::process::{Command, Stdio};

fn lex_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lex")
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(lex_bin())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lex");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
    )
}

fn write_to_tempfile(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lex-check-rules-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, src).unwrap();
    path
}

#[test]
fn type_error_envelope_carries_rule_tag_and_explanation() {
    // Return-type mismatch — body produces Str, signature claims Int.
    let path = write_to_tempfile("bad.lex", "fn bad(x :: Int) -> Int { \"oops\" }\n");
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 2, "type error must exit nonzero: {stdout}");

    let env: serde_json::Value = serde_json::from_str(&stdout).expect("envelope parses");
    let err = env.pointer("/data/errors/0").expect("first error present");
    assert_eq!(
        err.get("rule_tag").and_then(|v| v.as_str()),
        Some("type-mismatch"),
        "rule_tag must be the stable kebab-case identifier: {err}"
    );
    let expl = err
        .get("rule_explanation")
        .and_then(|v| v.as_str())
        .expect("rule_explanation present");
    assert!(
        expl.len() > 40,
        "rule_explanation must be non-trivial prose, got: {expl}"
    );
}

#[test]
fn rule_tag_disambiguates_two_different_errors() {
    // Two functions, two distinct rules: type-mismatch + unknown-identifier.
    let src = "\
fn one(x :: Int) -> Str { x }
fn two() -> Int { undefined_var }
";
    let path = write_to_tempfile("twoerrs.lex", src);
    let (code, stdout) = run(&["--output", "json", "check", path.to_str().unwrap()]);
    assert_eq!(code, 2);

    let env: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = env
        .pointer("/data/errors")
        .and_then(|v| v.as_array())
        .expect("errors array");
    let tags: Vec<&str> = arr
        .iter()
        .filter_map(|e| e.get("rule_tag").and_then(|v| v.as_str()))
        .collect();
    assert!(
        tags.contains(&"type-mismatch"),
        "expected a type-mismatch in {:?}",
        tags
    );
    assert!(
        tags.contains(&"unknown-identifier"),
        "expected an unknown-identifier in {:?}",
        tags
    );
}

#[test]
fn lex_docs_rules_lists_every_rule() {
    let (code, stdout) = run(&["--output", "json", "docs", "--rules"]);
    assert_eq!(code, 0, "docs --rules: {stdout}");

    let env: serde_json::Value = serde_json::from_str(&stdout).expect("envelope parses");
    let arr = env
        .pointer("/data/rules")
        .and_then(|v| v.as_array())
        .expect("rules array present");
    let tags: std::collections::BTreeSet<&str> = arr
        .iter()
        .filter_map(|r| r.get("rule_tag").and_then(|v| v.as_str()))
        .collect();
    // At least the canonical rule_tags must be present. Adding new
    // tags should grow the list, never shrink it.
    for required in [
        "type-mismatch",
        "unknown-identifier",
        "arity-mismatch",
        "non-exhaustive-match",
        "unknown-field",
        "effect-not-declared",
    ] {
        assert!(
            tags.contains(required),
            "rule_tag `{required}` missing from catalog: {tags:?}"
        );
    }
    // Every rule must carry a non-empty explanation.
    for r in arr {
        let expl = r.get("rule_explanation").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            !expl.is_empty(),
            "rule_explanation missing on {}",
            r.get("rule_tag").and_then(|v| v.as_str()).unwrap_or("?")
        );
    }
}

#[test]
fn lex_docs_rules_text_render_lists_every_tag() {
    let (code, stdout) = run(&["docs", "--rules"]);
    assert_eq!(code, 0);
    for tag in ["type-mismatch", "effect-not-declared", "non-exhaustive-match"] {
        assert!(
            stdout.contains(tag),
            "text render missing `{tag}`: {stdout}"
        );
    }
}
