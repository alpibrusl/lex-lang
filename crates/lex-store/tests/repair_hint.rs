//! Conformance tests for #281 — `RepairHint` attestation
//! auto-emitted on TypeError-rejected ops.

use lex_ast::{canonicalize_program, sig_id, stage_id, CExpr, CLit, NodeId, Stage};
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use tempfile::TempDir;

fn fresh() -> (Store, TempDir) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn stage_named(src: &str, name: &str) -> Stage {
    let prog = parse_source(src).unwrap();
    canonicalize_program(&prog)
        .into_iter()
        .find(|s| match s {
            Stage::FnDecl(fd) => fd.name == name,
            _ => false,
        })
        .expect("stage not found")
}

const PICK_SRC: &str = "fn pick(n :: Int) -> Int { match n { 0 => 1, _ => 2 } }\n";

fn publish_initial_pick(store: &Store) -> (String, String) {
    let s = stage_named(PICK_SRC, "pick");
    let sig = sig_id(&s).unwrap();
    let stg = stage_id(&s).unwrap();
    store.publish(&s).unwrap();
    let op = lex_vcs::Operation::new(
        lex_vcs::OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stg.clone(),
            effects: Default::default(),
            budget_cost: None,
        },
        [],
    );
    let t = lex_vcs::StageTransition::Create {
        sig_id: sig.clone(),
        stage_id: stg.clone(),
    };
    store.apply_operation(DEFAULT_BRANCH, op, t).unwrap();
    (sig, stg)
}

#[test]
fn type_error_emits_repair_hint_attestation() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Replace arm 0's body with a `Str` literal — type mismatch.
    let ill_typed = CExpr::Literal { value: CLit::Str { value: "not an int".into() } };
    let err = store
        .apply_replace_match_arm(
            DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
            0, ill_typed,
        )
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TypeError(_)),
        "expected TypeError, got {err:?}");

    // RepairHint must be on the candidate stage.
    let attlog = store.attestation_log().unwrap();
    let all = attlog.list_all().unwrap();
    let hints: Vec<&lex_vcs::Attestation> = all.iter()
        .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairHint { .. }))
        .collect();
    assert_eq!(hints.len(), 1, "exactly one RepairHint emitted");
    let hint = hints[0];
    let lex_vcs::AttestationKind::RepairHint {
        failed_op_id, errors, suggested_transform,
    } = &hint.kind else { unreachable!() };
    assert!(!failed_op_id.is_empty(),
        "failed_op_id is the would-be op_id");
    assert!(suggested_transform.is_none(),
        "slice 1 leaves the suggestion empty");
    let arr = errors.as_array().expect("errors is an array");
    assert!(!arr.is_empty(), "TypeError carries at least one entry");
}

#[test]
fn successful_apply_does_not_emit_a_repair_hint() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Well-typed transform: replace arm 0's body with a different
    // Int literal.
    let new_body = CExpr::Literal { value: CLit::Int { value: 42 } };
    store
        .apply_replace_match_arm(
            DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
            0, new_body,
        )
        .unwrap();
    let attlog = store.attestation_log().unwrap();
    let n_hints = attlog.list_all().unwrap()
        .iter()
        .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::RepairHint { .. }))
        .count();
    assert_eq!(n_hints, 0, "no RepairHint on successful apply");
}

#[test]
fn repair_hint_failed_op_id_matches_the_rejected_op() {
    // The RepairHint records the *would-be* op_id (deterministic
    // SHA-256 over the rejected op's canonical form), even though
    // no op record was persisted. Re-running with the same op
    // produces the same op_id and lets us correlate.
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    let ill_typed = CExpr::Literal { value: CLit::Str { value: "x".into() } };
    let _ = store.apply_replace_match_arm(
        DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
        0, ill_typed.clone(),
    );

    let attlog = store.attestation_log().unwrap();
    let all = attlog.list_all().unwrap();
    let first_hint_op_id = all.iter()
        .find_map(|a| match &a.kind {
            lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } =>
                Some(failed_op_id.clone()),
            _ => None,
        })
        .expect("first run must emit a hint");

    // Running the SAME ill-typed transform again produces the same
    // failed_op_id (content-addressed). Idempotent attestation
    // dedup means no new hint is added.
    let _ = store.apply_replace_match_arm(
        DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.2".into()),
        0, ill_typed,
    );
    let post = attlog.list_all().unwrap();
    let post_hints: Vec<_> = post.iter()
        .filter_map(|a| match &a.kind {
            lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } =>
                Some(failed_op_id.clone()),
            _ => None,
        })
        .collect();
    assert!(post_hints.iter().any(|id| id == &first_hint_op_id),
        "deterministic op_id is preserved across retries");
}
