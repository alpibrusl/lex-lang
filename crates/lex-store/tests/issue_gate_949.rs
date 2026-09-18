//! #949 phase 2: the acceptance evaluator + `IssueVerified` attestation —
//! the definition of done as code. A typed delta is checked against the head
//! like-with-like (rendered signatures, whitespace-insensitive); an issue's
//! examples are attached to the head with every other example stripped; the
//! verdict is recorded keyed by the issue id and "done" is a lookup.

use std::collections::{BTreeMap, BTreeSet};

use lex_ast::canonicalize_program;
use lex_store::issues::{
    check_api_delta, evaluate_static, is_verified, issue_verdicts, prepare_example_stages,
    record_issue_verdict, IssueEvaluation,
};
use lex_store::{Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_vcs::{Acceptance, ApiChangeKind, ApiEntry, AttestationKind, AttestationResult, Issue};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    canonicalize_program(&parse_source(src).expect("parse"))
}

fn publish(store: &Store, src: &str) -> String {
    let stages = parse(src);
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&BTreeMap::new(), &new, &et, &et, true);
    store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &lex_vcs::ImportMap::new(), true)
        .expect("publish")
        .head_op
        .expect("head op")
}

const HEAD_SRC: &str = "fn gcd(a :: Int, b :: Int) -> Int { if b == 0 { a } else { gcd(b, a % b) } }\n\
                        fn twice(n :: Int) -> Int { n + n }\n";

fn entry(name: &str, sig: &str, kind: ApiChangeKind) -> ApiEntry {
    ApiEntry { name: name.into(), signature: sig.into(), kind }
}

fn issue(acceptance: Acceptance) -> Issue {
    Issue::with_timestamp("t", "", acceptance, None, BTreeSet::new(), None, 1)
}

#[test]
fn typed_delta_added_matches_head_whitespace_insensitively() {
    let (store, _t) = fresh();
    let head = publish(&store, HEAD_SRC);
    let api = vec![entry("gcd", "(a :: Int, b :: Int) -> Int", ApiChangeKind::Added)];
    assert_eq!(check_api_delta(&store, None, &head, &api).unwrap(), IssueEvaluation::Passed);
    let api = vec![entry("gcd", "(a::Int,b::Int)->Int", ApiChangeKind::Added)];
    assert_eq!(check_api_delta(&store, None, &head, &api).unwrap(), IssueEvaluation::Passed);
}

#[test]
fn typed_delta_signature_mismatch_fails_with_detail() {
    let (store, _t) = fresh();
    let head = publish(&store, HEAD_SRC);
    let api = vec![entry("gcd", "(a :: Int) -> Int", ApiChangeKind::Added)];
    match check_api_delta(&store, None, &head, &api).unwrap() {
        IssueEvaluation::Failed { detail } => assert!(detail.contains("differs"), "{detail}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn typed_delta_absent_and_still_present_fail() {
    let (store, _t) = fresh();
    let head = publish(&store, HEAD_SRC);
    // Declared added, but nothing named lcm exists.
    let api = vec![entry("lcm", "(Int, Int) -> Int", ApiChangeKind::Added)];
    match check_api_delta(&store, None, &head, &api).unwrap() {
        IssueEvaluation::Failed { detail } => assert!(detail.contains("absent at head"), "{detail}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    // Declared removed, but gcd is still there.
    let api = vec![entry("gcd", "", ApiChangeKind::Removed)];
    match check_api_delta(&store, None, &head, &api).unwrap() {
        IssueEvaluation::Failed { detail } => assert!(detail.contains("still present"), "{detail}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn static_evaluation_per_shape() {
    let (store, _t) = fresh();
    let head = publish(&store, HEAD_SRC);
    // Free-form is human-closed: never machine-evaluable.
    let ff = issue(Acceptance::FreeForm {});
    assert!(matches!(
        evaluate_static(&store, &ff, &head).unwrap(),
        IssueEvaluation::NotEvaluable { .. }
    ));
    // Metric / evidence oracles land in #954: inconclusive, not passed.
    let m = issue(Acceptance::MetricInvariant { predicate: "p99 < 200".into(), window: "7d".into() });
    assert!(matches!(evaluate_static(&store, &m, &head).unwrap(), IssueEvaluation::NotEvaluable { .. }));
    // A failing example has nothing static; the example run decides.
    let fe = issue(Acceptance::FailingExample { example: "gcd(0, 0) => 0".into() });
    assert_eq!(evaluate_static(&store, &fe, &head).unwrap(), IssueEvaluation::Passed);
}

/// Parse one example case the way a caller does: under a stub fn (the parser
/// keeps a case's args + expected, not its callee).
fn parse_example(name: &str, case: &str) -> lex_ast::Example {
    let src = format!("fn {name}() -> Int examples {{ {case} }} {{ 0 }}\n");
    let stages = parse(&src);
    stages
        .into_iter()
        .find_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => fd.examples.into_iter().next(),
            _ => None,
        })
        .expect("one example")
}

#[test]
fn prepare_example_stages_attaches_only_the_issue_cases() {
    let (store, _t) = fresh();
    let head = publish(&store, HEAD_SRC);
    let ex = parse_example("gcd", "gcd(12, 8) => 4");
    let stages = prepare_example_stages(&store, &head, &[("gcd".to_string(), ex.clone())]).unwrap();
    let counts: BTreeMap<String, usize> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.examples.len())),
            _ => None,
        })
        .collect();
    assert_eq!(counts.get("gcd"), Some(&1), "the issue's case is attached to gcd");
    assert_eq!(counts.get("twice"), Some(&0), "every pre-existing example is stripped");
    // A case targeting a function the head doesn't declare is a typed error.
    let missing = prepare_example_stages(&store, &head, &[("lcm".to_string(), ex)]);
    assert!(matches!(missing, Err(StoreError::IssueTarget(ref n)) if n == "lcm"));
}

#[test]
fn verdict_is_recorded_keyed_by_issue_and_done_is_a_lookup() {
    let (store, _t) = fresh();
    let head = publish(&store, HEAD_SRC);

    // A pass is recorded as Passed and makes the issue verified.
    let ok = issue(Acceptance::TypedDelta {
        api: vec![entry("gcd", "(a :: Int, b :: Int) -> Int", ApiChangeKind::Added)],
        examples: vec![],
    });
    let eval = evaluate_static(&store, &ok, &head).unwrap();
    assert!(eval.is_passed());
    record_issue_verdict(&store, &ok, &head, &eval).unwrap();
    let vs = issue_verdicts(&store, &ok.issue_id).unwrap();
    assert_eq!(vs.len(), 1);
    assert!(matches!(&vs[0].kind, AttestationKind::IssueVerified { shape, .. } if shape == "typed_delta"));
    assert_eq!(vs[0].result, AttestationResult::Passed);
    assert_eq!(vs[0].op_id.as_deref(), Some(head.as_str()));
    assert!(is_verified(&store, &ok.issue_id).unwrap());

    // A failure is recorded as Failed and is not "done".
    let bad = issue(Acceptance::TypedDelta {
        api: vec![entry("lcm", "(Int, Int) -> Int", ApiChangeKind::Added)],
        examples: vec![],
    });
    let eval = evaluate_static(&store, &bad, &head).unwrap();
    record_issue_verdict(&store, &bad, &head, &eval).unwrap();
    assert!(!is_verified(&store, &bad.issue_id).unwrap());

    // Free-form is never a machine pass: recorded Inconclusive.
    let ff = issue(Acceptance::FreeForm {});
    let eval = evaluate_static(&store, &ff, &head).unwrap();
    record_issue_verdict(&store, &ff, &head, &eval).unwrap();
    let vs = issue_verdicts(&store, &ff.issue_id).unwrap();
    assert!(matches!(vs[0].result, AttestationResult::Inconclusive { .. }));
    assert!(!is_verified(&store, &ff.issue_id).unwrap());

    // Unrelated issue: no verdicts, not done.
    assert!(issue_verdicts(&store, "nope").unwrap().is_empty());
    assert!(!is_verified(&store, "nope").unwrap());
}
