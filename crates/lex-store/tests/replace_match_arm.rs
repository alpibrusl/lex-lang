//! Conformance tests for #280: typed `ReplaceMatchArm` transform
//! + matching `OperationKind::ReplaceMatchArm` op.

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

/// `pick` matches on `n` and returns 1 for 0, else returns 2.
const PICK_SRC: &str = "fn pick(n :: Int) -> Int { match n { 0 => 1, _ => 2 } }\n";

/// Find the `NodeId` of the `Match` expression in `pick`'s body.
/// For a 1-param FnDecl the body is at `n_0.2`, and `pick`'s body
/// is *literally* the match — no surrounding block — so `n_0.2`
/// addresses the Match itself.
fn pick_match_node() -> NodeId {
    NodeId("n_0.2".into())
}

fn publish_initial_pick(store: &Store) -> (String, String) {
    // Publish + add to the branch via apply_operation. Returns
    // (sig_id, stage_id).
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
fn replace_match_arm_lands_a_typed_op_on_the_branch() {
    let (store, _tmp) = fresh();
    let (sig, from_stage_id) = publish_initial_pick(&store);

    // Replace the first arm's body (0 → 1) with (0 → 42).
    let new_body = CExpr::Literal { value: CLit::Int { value: 42 } };
    let op_id = store
        .apply_replace_match_arm(DEFAULT_BRANCH, &from_stage_id, &pick_match_node(), 0, new_body)
        .expect("transform should apply cleanly on well-typed input");

    // Op record exists; transition is Replace; kind is ReplaceMatchArm.
    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::ReplaceMatchArm {
        sig_id: rec_sig,
        from_stage_id: rec_from,
        arm_index,
        match_node,
        ..
    } = &rec.op.kind else {
        panic!("expected ReplaceMatchArm op kind, got {:?}", rec.op.kind);
    };
    assert_eq!(rec_sig, &sig);
    assert_eq!(rec_from, &from_stage_id);
    assert_eq!(*arm_index, 0);
    assert_eq!(match_node, "n_0.2");

    // Branch head updated.
    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_ne!(head.get(&sig), Some(&from_stage_id));
}

#[test]
fn replace_match_arm_get_ast_reconstructs_new_body() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    let new_body = CExpr::Literal { value: CLit::Int { value: 7 } };
    let op_id = store
        .apply_replace_match_arm(DEFAULT_BRANCH, &from_stage_id, &pick_match_node(), 0, new_body)
        .unwrap();

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::ReplaceMatchArm { to_stage_id, .. } = &rec.op.kind else {
        panic!();
    };

    let reloaded = store.get_ast(to_stage_id).unwrap();
    let Stage::FnDecl(fd) = reloaded else { panic!() };
    let CExpr::Match { arms, .. } = fd.body else { panic!() };
    // First arm body was replaced.
    assert!(matches!(arms[0].body, CExpr::Literal { value: CLit::Int { value: 7 } }));
    // Second arm untouched.
    assert!(matches!(arms[1].body, CExpr::Literal { value: CLit::Int { value: 2 } }));
}

#[test]
fn replace_match_arm_emits_typecheck_attestation() {
    // The transform path goes through `apply_operation_checked`,
    // which emits a TypeCheck::Passed attestation against the new
    // stage. Verify the attestation lands.
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    let new_body = CExpr::Literal { value: CLit::Int { value: 99 } };
    let op_id = store
        .apply_replace_match_arm(DEFAULT_BRANCH, &from_stage_id, &pick_match_node(), 0, new_body)
        .unwrap();

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::ReplaceMatchArm { to_stage_id, .. } = &rec.op.kind else {
        panic!();
    };

    let attlog = store.attestation_log().unwrap();
    let by_stage = attlog.list_for_stage(to_stage_id).unwrap();
    assert!(
        by_stage.iter().any(|a| matches!(a.kind, lex_vcs::AttestationKind::TypeCheck)),
        "expected a TypeCheck attestation against the new stage"
    );
}

#[test]
fn replace_match_arm_rejects_ill_typed_result() {
    // Replace the first arm's body (an Int) with a string literal.
    // The transform succeeds (it's a pure AST edit) but the
    // re-typecheck in `apply_operation_checked` rejects with
    // `StoreError::TypeError`. Branch head unchanged.
    let (store, _tmp) = fresh();
    let (sig, from_stage_id) = publish_initial_pick(&store);
    let head_before = store.branch_head(DEFAULT_BRANCH).unwrap();

    let ill_typed = CExpr::Literal { value: CLit::Str { value: "not an int".into() } };
    let err = store
        .apply_replace_match_arm(DEFAULT_BRANCH, &from_stage_id, &pick_match_node(), 0, ill_typed)
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TypeError(_)),
        "expected TypeError, got {err:?}");

    let head_after = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head_after.get(&sig), head_before.get(&sig),
        "branch head must be unchanged after a rejected transform");
}

#[test]
fn replace_match_arm_surface_transform_errors() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Arm index out of range.
    let err = store
        .apply_replace_match_arm(
            DEFAULT_BRANCH, &from_stage_id, &pick_match_node(),
            99, CExpr::Literal { value: CLit::Int { value: 0 } },
        )
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TransformError(_)),
        "expected TransformError, got {err:?}");
}

#[test]
fn replace_match_arm_no_op_is_rejected() {
    // Replace arm 0's body with the IDENTICAL body. The transform
    // produces the same stage_id; we refuse the empty edit rather
    // than advance the branch with a redundant op.
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_pick(&store);

    // Arm 0's original body was `1`; replace with `1`.
    let same = CExpr::Literal { value: CLit::Int { value: 1 } };
    let err = store
        .apply_replace_match_arm(DEFAULT_BRANCH, &from_stage_id, &pick_match_node(), 0, same)
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::InvalidTransition(_)),
        "expected InvalidTransition for no-op transform, got {err:?}");
}
