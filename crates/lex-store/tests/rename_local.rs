//! Conformance tests for #280 slice 2: typed `RenameLocal`
//! transform + matching `OperationKind::RenameLocal` op.

use lex_ast::{canonicalize_program, sig_id, stage_id, CExpr, NodeId, Stage};
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

const ADD_TWO_SRC: &str =
    "fn add_two(n :: Int) -> Int { let x := n + 1; x + 1 }\n";

/// `add_two` has one param + body = let at NodeId `n_0.2`.
fn let_node() -> NodeId { NodeId("n_0.2".into()) }

fn publish_initial_add_two(store: &Store) -> (String, String) {
    let s = stage_named(ADD_TWO_SRC, "add_two");
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
fn rename_local_lands_a_typed_op_on_the_branch() {
    let (store, _tmp) = fresh();
    let (sig, from_stage_id) = publish_initial_add_two(&store);

    let op_id = store
        .apply_rename_local(DEFAULT_BRANCH, &from_stage_id, &let_node(), "y")
        .expect("rename should succeed on well-typed input");

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::RenameLocal {
        sig_id: rec_sig,
        from_stage_id: rec_from,
        let_node,
        old_name,
        new_name,
        ..
    } = &rec.op.kind else {
        panic!("expected RenameLocal op kind, got {:?}", rec.op.kind);
    };
    assert_eq!(rec_sig, &sig);
    assert_eq!(rec_from, &from_stage_id);
    assert_eq!(let_node, "n_0.2");
    assert_eq!(old_name, "x");
    assert_eq!(new_name, "y");

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_ne!(head.get(&sig), Some(&from_stage_id));
}

#[test]
fn rename_local_reconstructs_through_get_ast() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_add_two(&store);

    let op_id = store
        .apply_rename_local(DEFAULT_BRANCH, &from_stage_id, &let_node(), "y")
        .unwrap();
    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::RenameLocal { to_stage_id, .. } = &rec.op.kind else {
        panic!();
    };

    let reloaded = store.get_ast(to_stage_id).unwrap();
    let Stage::FnDecl(fd) = reloaded else { panic!() };
    let CExpr::Let { name, body, .. } = fd.body else { panic!() };
    assert_eq!(name, "y", "binding renamed");
    // body's LHS now references `y`.
    let CExpr::BinOp { lhs, .. } = *body else { panic!() };
    assert!(matches!(*lhs, CExpr::Var { name: ref n } if n == "y"));
}

#[test]
fn rename_local_refuses_no_op() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_add_two(&store);

    let err = store
        .apply_rename_local(DEFAULT_BRANCH, &from_stage_id, &let_node(), "x")
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TransformError(
        lex_ast::TransformError::RenameNoOp { .. }
    )), "got {err:?}");
}

#[test]
fn rename_local_emits_typecheck_attestation() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_add_two(&store);

    let op_id = store
        .apply_rename_local(DEFAULT_BRANCH, &from_stage_id, &let_node(), "y")
        .unwrap();

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::RenameLocal { to_stage_id, .. } = &rec.op.kind else {
        panic!();
    };
    let attlog = store.attestation_log().unwrap();
    let by_stage = attlog.list_for_stage(to_stage_id).unwrap();
    assert!(
        by_stage.iter().any(|a| matches!(a.kind, lex_vcs::AttestationKind::TypeCheck)),
        "TypeCheck attestation should accompany the renamed stage"
    );
}

#[test]
fn rename_local_surfaces_transform_errors_for_wrong_node() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial_add_two(&store);

    // Target a NodeId that doesn't address a Let.
    let err = store
        .apply_rename_local(DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.99".into()), "y")
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TransformError(_)),
        "got {err:?}");
}
