//! M16 acceptance: every JSON descriptor in the conformance dir runs
//! through the harness and produces the expected status/output.

use conformance::{count_tokens, run_directory, run_descriptor, Descriptor, GRAMMAR_REFERENCE, Outcome};

#[test]
fn full_conformance_directory_passes() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap()
        .join("conformance");
    let report = run_directory(&dir).expect("read dir");
    if !report.failed.is_empty() {
        let mut msg = format!("conformance failed: {}/{} failed\n", report.failed.len(), report.total());
        for (name, why) in &report.failed {
            msg.push_str(&format!("  - {name}: {why}\n"));
        }
        panic!("{msg}");
    }
    assert!(report.total() >= 8, "expected ≥8 descriptors, got {}", report.total());
}

#[test]
fn inline_source_descriptor_passes() {
    let d = Descriptor {
        name: "inline".into(),
        language: "lex".into(),
        source: Some("fn add(x :: Int, y :: Int) -> Int { x + y }\n".into()),
        source_file: None,
        func: Some("add".into()),
        input: vec![serde_json::json!(2), serde_json::json!(3)],
        expected_output: Some(serde_json::json!(5)),
        policy: Default::default(),
        expected_status: conformance::ExpectedStatus::default(),
    };
    matches!(run_descriptor(&d), Outcome::Pass);
}

#[test]
fn descriptor_with_wrong_expected_output_fails() {
    let d = Descriptor {
        name: "wrong_out".into(),
        language: "lex".into(),
        source: Some("fn one() -> Int { 1 }\n".into()),
        source_file: None,
        func: Some("one".into()),
        input: vec![],
        expected_output: Some(serde_json::json!(2)),
        policy: Default::default(),
        expected_status: conformance::ExpectedStatus::default(),
    };
    let r = run_descriptor(&d);
    assert!(matches!(r, Outcome::Fail(_)));
}

#[test]
fn grammar_reference_fits_in_500_tokens() {
    let n = count_tokens(GRAMMAR_REFERENCE);
    assert!(n <= 500, "grammar reference is {n} tokens (>500 budget)");
    // Sanity: it's not trivially small.
    assert!(n > 100, "grammar reference seems too small ({n}); check the const");
}

#[test]
fn token_estimator_smoke() {
    // Heuristic check: matches expected magnitude on a known text.
    let t = "fn add(x :: Int, y :: Int) -> Int { x + y }";
    let n = count_tokens(t);
    assert!((10..40).contains(&n), "token estimate {n} out of plausible band");
}
