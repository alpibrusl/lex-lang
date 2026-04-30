//! M10 acceptance per spec §14.5.

use spec_checker::{check_spec, parse_spec, to_smtlib, ProofStatus};

const CLAMP_GOOD: &str = r#"
fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {
  match x < lo {
    true => lo,
    false => match x > hi {
      true => hi,
      false => x,
    },
  }
}
"#;

const CLAMP_BAD: &str = r#"
fn clamp(x :: Int, lo :: Int, hi :: Int) -> Int {
  match x < lo {
    true => x,
    false => match x > hi {
      true => x,
      false => x,
    },
  }
}
"#;

const CLAMP_SPEC: &str = r#"
spec clamp {
  forall x :: Int, lo :: Int, hi :: Int where lo <= hi:
    let r := clamp(x, lo, hi)
    (r >= lo) and (r <= hi)
}
"#;

#[test]
fn parse_round_trip() {
    let spec = parse_spec(CLAMP_SPEC).expect("parse");
    assert_eq!(spec.name, "clamp");
    assert_eq!(spec.quantifiers.len(), 3);
    assert_eq!(spec.quantifiers[0].name, "x");
    assert!(spec.quantifiers[2].constraint.is_some(), "where attaches to last quantifier");
}

#[test]
fn good_clamp_is_proved() {
    let spec = parse_spec(CLAMP_SPEC).expect("parse");
    let r = check_spec(&spec, CLAMP_GOOD, 1000);
    assert_eq!(r.status, ProofStatus::Proved, "expected proved, evidence: {:?}", r.evidence);
    assert_eq!(r.evidence.method, "randomized");
    assert_eq!(r.evidence.trials, 1000);
}

#[test]
fn bad_clamp_returns_counterexample() {
    let spec = parse_spec(CLAMP_SPEC).expect("parse");
    let r = check_spec(&spec, CLAMP_BAD, 1000);
    assert_eq!(r.status, ProofStatus::Counterexample, "expected counterexample, got {:?}", r.status);
    let cx = r.evidence.counterexample.expect("counterexample bindings");
    assert!(cx.contains_key("x") && cx.contains_key("lo") && cx.contains_key("hi"),
        "counterexample must include all quantifier bindings: {cx:?}");
}

#[test]
fn float_quantifier_reports_inconclusive() {
    let src = r#"
spec id_float {
  forall x :: Float:
    (x == x) or true
}
"#;
    let spec = parse_spec(src).expect("parse");
    // Even with a trivially-true property, the randomized strategy bails
    // early on Float quantifiers and reports inconclusive per §14.5.
    let r = check_spec(&spec, "fn unused() -> Int { 0 }\n", 100);
    assert_eq!(r.status, ProofStatus::Inconclusive);
    assert!(r.evidence.note.as_deref().unwrap_or("").contains("Float"),
        "expected note to mention Float; got {:?}", r.evidence.note);
}

#[test]
fn spec_id_is_deterministic() {
    let s1 = parse_spec(CLAMP_SPEC).unwrap();
    let s2 = parse_spec(CLAMP_SPEC).unwrap();
    let r1 = check_spec(&s1, CLAMP_GOOD, 10);
    let r2 = check_spec(&s2, CLAMP_GOOD, 10);
    assert_eq!(r1.spec_id, r2.spec_id, "spec_id must be stable for the same spec");
    assert_eq!(r1.spec_id.len(), 64, "spec_id is hex-encoded SHA-256");
}

#[test]
fn smtlib_export_includes_quantifiers_and_body() {
    let spec = parse_spec(CLAMP_SPEC).unwrap();
    let smt = to_smtlib(&spec);
    // Header
    assert!(smt.contains("(set-logic ALL)"));
    // Quantifiers as forall vars
    assert!(smt.contains("(x Int)"));
    assert!(smt.contains("(lo Int)"));
    assert!(smt.contains("(hi Int)"));
    // The implication wrapper
    assert!(smt.contains("(=> "));
    // Where-clause appears as antecedent
    assert!(smt.contains("(<= lo hi)"));
    // The clamp call is exposed as an uninterpreted function
    assert!(smt.contains("(declare-fun clamp"));
    assert!(smt.contains("(check-sat)"));
}

#[test]
fn check_passes_when_constraint_filters_all_inputs() {
    // Constraint `false` is unsatisfiable, so no inputs survive — the
    // spec is vacuously true. Survives the trial loop without finding
    // any counterexample → proved.
    let src = r#"
spec vacuous {
  forall x :: Int where false:
    false
}
"#;
    let spec = parse_spec(src).expect("parse");
    let r = check_spec(&spec, "fn unused() -> Int { 0 }\n", 100);
    assert_eq!(r.status, ProofStatus::Proved);
}
