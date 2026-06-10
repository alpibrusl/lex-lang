//! Conformance tests for #293 — `ProducerTrust` + `TrustWaived`.

use lex_store::{
    policy::{AttestationCondition, PolicyFile, RequiredAttestation, RequiredAttestationKind},
    Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH,
};
use std::collections::BTreeSet;
use tempfile::TempDir;

fn fresh() -> (Store, TempDir) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn producer(tool: &str) -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: tool.into(),
        version: "test".into(),
        model: None,
    }
}

fn push_attestation(
    store: &Store,
    stage: &str,
    tool: &str,
    kind: lex_vcs::AttestationKind,
    result: lex_vcs::AttestationResult,
) {
    let att = lex_vcs::Attestation::new(
        stage.to_string(),
        None,
        None,
        kind,
        result,
        producer(tool),
        None,
    );
    store.attestation_log().unwrap().put(&att).unwrap();
}

#[test]
fn recompute_with_no_attestations_returns_none() {
    let (store, _tmp) = fresh();
    let r = store
        .recompute_producer_trust("spec-checker", 1000, "tester")
        .unwrap();
    assert!(r.is_none(), "new producer has no evidence to score");
}

#[test]
fn recompute_with_all_passing_scores_full_trust() {
    let (store, _tmp) = fresh();
    for i in 0..10 {
        push_attestation(
            &store,
            &format!("stg-{i}"),
            "trustworthy",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    let id = store
        .recompute_producer_trust("trustworthy", 1000, "tester")
        .unwrap()
        .expect("trust attestation emitted");
    // Read back the attestation and verify score = 1000.
    let stored = store.attestation_log().unwrap().get(&id).unwrap().unwrap();
    let lex_vcs::AttestationKind::ProducerTrust {
        score_thousandths, ..
    } = stored.kind
    else {
        panic!("expected ProducerTrust");
    };
    assert_eq!(score_thousandths, 1000, "10/10 passed → score = 1.000");
}

#[test]
fn recompute_half_passing_scores_half() {
    let (store, _tmp) = fresh();
    for i in 0..5 {
        push_attestation(
            &store,
            &format!("stg-{i}"),
            "shaky",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    for i in 5..10 {
        push_attestation(
            &store,
            &format!("stg-{i}"),
            "shaky",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Failed {
                detail: "bad".into(),
            },
        );
    }
    let id = store
        .recompute_producer_trust("shaky", 1000, "tester")
        .unwrap()
        .unwrap();
    let stored = store.attestation_log().unwrap().get(&id).unwrap().unwrap();
    let lex_vcs::AttestationKind::ProducerTrust {
        score_thousandths, ..
    } = stored.kind
    else {
        panic!();
    };
    assert_eq!(score_thousandths, 500, "5/10 → 0.5");
}

#[test]
fn recompute_refuses_blocked_producer() {
    let (store, _tmp) = fresh();
    push_attestation(
        &store,
        "compromised-tool",
        "compromised-tool",
        lex_vcs::AttestationKind::ProducerBlock {
            tool_id: "compromised-tool".into(),
            reason: "rooted".into(),
            blocked_at: 1234,
        },
        lex_vcs::AttestationResult::Passed,
    );
    let err = store
        .recompute_producer_trust("compromised-tool", 1000, "tester")
        .unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidTransition(_)),
        "blocked producer should be refused; got {err:?}"
    );
}

#[test]
fn recompute_ignores_self_referential_trust_attestations() {
    // Re-running recompute on a producer should look at their
    // evidence attestations, not their previous trust score —
    // otherwise scores would compound circularly.
    let (store, _tmp) = fresh();
    for i in 0..3 {
        push_attestation(
            &store,
            &format!("stg-{i}"),
            "iterating",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    let _ = store
        .recompute_producer_trust("iterating", 1000, "tester")
        .unwrap()
        .unwrap();
    // Re-run; should still be 1000 (not influenced by the prior
    // ProducerTrust attestation in the log).
    let id2 = store
        .recompute_producer_trust("iterating", 1000, "tester")
        .unwrap()
        .unwrap();
    let stored = store.attestation_log().unwrap().get(&id2).unwrap().unwrap();
    let lex_vcs::AttestationKind::ProducerTrust {
        score_thousandths, ..
    } = stored.kind
    else {
        panic!();
    };
    assert_eq!(
        score_thousandths, 1000,
        "self-referential trust must not affect the recompute"
    );
}

#[test]
fn live_producer_trust_scores_reports_latest_live_per_producer() {
    let (store, _tmp) = fresh();
    // rocksolid: 10/10 → 1000; shaky: 5/10 → 500.
    for i in 0..10 {
        push_attestation(
            &store,
            &format!("a-{i}"),
            "rocksolid",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    for i in 0..5 {
        push_attestation(
            &store,
            &format!("b-{i}"),
            "shaky",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    for i in 5..10 {
        push_attestation(
            &store,
            &format!("b-{i}"),
            "shaky",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Failed { detail: "x".into() },
        );
    }
    store
        .recompute_producer_trust("rocksolid", 1000, "admin")
        .unwrap()
        .unwrap();
    store
        .recompute_producer_trust("shaky", 1000, "admin")
        .unwrap()
        .unwrap();

    let scores = store.live_producer_trust_scores().unwrap();
    assert_eq!(scores.get("rocksolid"), Some(&1000), "10/10 → 1.000");
    assert_eq!(scores.get("shaky"), Some(&500), "5/10 → 0.500");

    // A block is a hard veto: the producer drops out of the live scores,
    // so an export can't keep trusting a key after it's been revoked.
    push_attestation(
        &store,
        "rocksolid",
        "admin",
        lex_vcs::AttestationKind::ProducerBlock {
            tool_id: "rocksolid".into(),
            reason: "key rotated".into(),
            blocked_at: 9999,
        },
        lex_vcs::AttestationResult::Passed,
    );
    let scores = store.live_producer_trust_scores().unwrap();
    assert!(
        !scores.contains_key("rocksolid"),
        "blocked producer must not appear"
    );
    assert_eq!(
        scores.get("shaky"),
        Some(&500),
        "unblocked producer unaffected"
    );
}

// ---- gate-waiver tests ------------------------------------------

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    let prog = lex_syntax::parse_source(src).expect("parse");
    lex_ast::canonicalize_program(&prog)
}

fn add_fn_op() -> (Operation, StageTransition) {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        [],
    );
    let t = StageTransition::Create {
        sig_id: "fac".into(),
        stage_id: "stg-1".into(),
    };
    (op, t)
}

#[test]
fn gate_waives_rule_when_trust_above_threshold() {
    let (store, _tmp) = fresh();
    // Seed a trusted producer with 10 passing attestations.
    for i in 0..10 {
        push_attestation(
            &store,
            &format!("seed-{i}"),
            "rocksolid",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    store
        .recompute_producer_trust("rocksolid", 1000, "admin")
        .unwrap()
        .unwrap();

    // Policy requires Spec attestations, with a trust waiver at 900/1000.
    let policy = PolicyFile {
        required_attestations: vec![RequiredAttestation {
            kind: RequiredAttestationKind::Spec,
            when: AttestationCondition::Always,
            skip_if_producer_trust_thousandths_above: Some(900),
        }],
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();

    // Apply an op with NO Spec attestation. Without the waiver this
    // would block; with the waiver it should advance.
    let candidate =
        parse("fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n");
    let (op, t) = add_fn_op();
    let op_id = store
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect("trust waiver should let the advance through");

    // TrustWaived attestation lands.
    let attlog = store.attestation_log().unwrap();
    let waivers: Vec<_> = attlog
        .list_all()
        .unwrap()
        .into_iter()
        .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::TrustWaived { .. }))
        .collect();
    assert_eq!(waivers.len(), 1, "exactly one TrustWaived attestation");
    let lex_vcs::AttestationKind::TrustWaived {
        producer,
        score_thousandths,
        threshold_thousandths,
        kind_tag,
    } = &waivers[0].kind
    else {
        unreachable!()
    };
    assert_eq!(producer, "rocksolid");
    assert_eq!(*score_thousandths, 1000);
    assert_eq!(*threshold_thousandths, 900);
    assert_eq!(kind_tag, "spec");
    assert_eq!(waivers[0].op_id.as_deref(), Some(op_id.as_str()));
}

#[test]
fn gate_does_not_waive_when_trust_at_or_below_threshold() {
    let (store, _tmp) = fresh();
    // Seed a producer with 50/50 attestations → score = 500.
    for i in 0..5 {
        push_attestation(
            &store,
            &format!("seed-{i}"),
            "shaky",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    for i in 5..10 {
        push_attestation(
            &store,
            &format!("seed-{i}"),
            "shaky",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Failed { detail: "x".into() },
        );
    }
    store
        .recompute_producer_trust("shaky", 1000, "admin")
        .unwrap()
        .unwrap();

    // Policy threshold is 900; shaky's score (500) is below.
    let policy = PolicyFile {
        required_attestations: vec![RequiredAttestation {
            kind: RequiredAttestationKind::Spec,
            when: AttestationCondition::Always,
            skip_if_producer_trust_thousandths_above: Some(900),
        }],
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();

    let candidate =
        parse("fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n");
    let (op, t) = add_fn_op();
    let err = store
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .unwrap_err();
    match err {
        StoreError::BranchAdvanceBlocked(_) => {}
        other => panic!("expected BranchAdvanceBlocked, got {other:?}"),
    }
}

#[test]
fn blocked_producer_does_not_count_toward_trust_waiver() {
    let (store, _tmp) = fresh();
    // Seed a producer with a high trust score…
    for i in 0..10 {
        push_attestation(
            &store,
            &format!("seed-{i}"),
            "compromised",
            lex_vcs::AttestationKind::TypeCheck,
            lex_vcs::AttestationResult::Passed,
        );
    }
    store
        .recompute_producer_trust("compromised", 1000, "admin")
        .unwrap()
        .unwrap();
    // …then block the producer retroactively.
    push_attestation(
        &store,
        "compromised",
        "admin",
        lex_vcs::AttestationKind::ProducerBlock {
            tool_id: "compromised".into(),
            reason: "found rooted".into(),
            blocked_at: 9999,
        },
        lex_vcs::AttestationResult::Passed,
    );

    // Policy with trust waiver. The blocked tool should NOT
    // count toward the trust ceiling, so the waiver doesn't fire.
    let policy = PolicyFile {
        required_attestations: vec![RequiredAttestation {
            kind: RequiredAttestationKind::Spec,
            when: AttestationCondition::Always,
            skip_if_producer_trust_thousandths_above: Some(900),
        }],
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();

    let candidate =
        parse("fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n");
    let (op, t) = add_fn_op();
    let err = store
        .apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .unwrap_err();
    assert!(
        matches!(err, StoreError::BranchAdvanceBlocked(_)),
        "blocked producer's trust must not waive the rule"
    );
}
