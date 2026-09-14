//! #836 G3: replay-as-verification.
//!
//! `replay_request` assembles what a regenerator needs (recorded
//! intent, target sig, expected stage, parent program); `replay_compare`
//! checks a regenerated candidate against the recorded stage and emits a
//! `Replay` attestation. The model call is external — these test the
//! deterministic halves lex owns.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn named(src: &str, name: &str) -> lex_ast::Stage {
    lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap())
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == name))
        .expect("fn not found")
}

/// Land an AddFunction, returning (op_id, sig, stage_id).
fn land_add(s: &Store, src: &str, name: &str) -> (String, String, String) {
    let st = named(src, name);
    let sig = lex_ast::sig_id(&st).unwrap();
    let stg = lex_ast::stage_id(&st).unwrap();
    s.publish(&st).unwrap();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stg.clone(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        s.get_branch(DEFAULT_BRANCH).unwrap().and_then(|b| b.head_op).into_iter().collect::<Vec<_>>(),
    );
    let t = StageTransition::Create { sig_id: sig.clone(), stage_id: stg.clone() };
    let op_id = s.apply_operation_gated(DEFAULT_BRANCH, op, t).expect("gated add");
    (op_id, sig, stg)
}

#[test]
fn replay_request_carries_target_sig_expected_stage_and_parent_program() {
    let (s, _tmp) = fresh();
    // A first fn so the second op has a non-empty parent program.
    land_add(&s, "fn helper(x :: Int) -> Int { x }\n", "helper");
    let (op_id, sig, stage) = land_add(&s, "fn helper(x :: Int) -> Int { x }\nfn g(x :: Int) -> Int { helper(x) }\n", "g");

    let req = s.replay_request(&op_id).unwrap();
    assert_eq!(req.op_id, op_id);
    assert_eq!(req.target_sig, sig);
    assert_eq!(req.expected_stage_id, stage);
    // g was added on top of helper → the parent program contains helper.
    assert!(req.parent_program.contains("helper"), "parent program: {}", req.parent_program);
    assert!(!req.parent_program.contains("fn g"), "parent must predate g: {}", req.parent_program);
}

#[test]
fn replay_compare_reproduced_when_candidate_matches() {
    let (s, _tmp) = fresh();
    let (op_id, _sig, stage) = land_add(&s, "fn f(x :: Int) -> Int { x + 1 }\n", "f");

    // A faithful regeneration: the identical function.
    let candidate = named("fn f(x :: Int) -> Int { x + 1 }\n", "f");
    let outcome = s.replay_compare(&op_id, &candidate).unwrap();
    assert!(outcome.reproduced, "identical regeneration must reproduce: {outcome:?}");
    assert_eq!(outcome.produced_stage_id.as_deref(), Some(stage.as_str()));

    // The Replay attestation is on the recorded stage, result Passed.
    let atts = s.attestation_log().unwrap().list_for_stage(&stage).unwrap();
    let replay = atts.iter().find(|a| matches!(a.kind, lex_vcs::AttestationKind::Replay { .. }))
        .expect("a Replay attestation");
    assert!(matches!(replay.result, lex_vcs::AttestationResult::Passed));
    assert!(matches!(&replay.kind, lex_vcs::AttestationKind::Replay { reproduced: true, .. }));
}

#[test]
fn replay_compare_not_reproduced_when_body_differs() {
    let (s, _tmp) = fresh();
    let (op_id, _sig, stage) = land_add(&s, "fn f(x :: Int) -> Int { x + 1 }\n", "f");

    // Same signature, different body → same sig_id, different stage_id.
    let candidate = named("fn f(x :: Int) -> Int { x + 2 }\n", "f");
    let outcome = s.replay_compare(&op_id, &candidate).unwrap();
    assert!(!outcome.reproduced, "a different body must not reproduce");
    assert!(outcome.produced_stage_id.is_some(), "same sig → a produced stage is recorded");
    assert_ne!(outcome.produced_stage_id.as_deref(), Some(stage.as_str()));

    let atts = s.attestation_log().unwrap().list_for_stage(&stage).unwrap();
    let replay = atts.iter().find(|a| matches!(a.kind, lex_vcs::AttestationKind::Replay { .. }))
        .expect("a Replay attestation");
    assert!(matches!(replay.result, lex_vcs::AttestationResult::Failed { .. }));
}

#[test]
fn replay_compare_different_sig_is_not_a_reproduction() {
    let (s, _tmp) = fresh();
    let (op_id, _sig, _stage) = land_add(&s, "fn f(x :: Int) -> Int { x + 1 }\n", "f");

    // A candidate for a wholly different function.
    let candidate = named("fn other(x :: Int) -> Int { x }\n", "other");
    let outcome = s.replay_compare(&op_id, &candidate).unwrap();
    assert!(!outcome.reproduced);
    assert!(outcome.produced_stage_id.is_none(), "a different sig produces nothing for this op");
}

#[test]
fn replay_request_on_unknown_op_errors() {
    let (s, _tmp) = fresh();
    assert!(s.replay_request("deadbeef").is_err());
}
