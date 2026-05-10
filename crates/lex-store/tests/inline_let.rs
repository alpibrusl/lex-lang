//! Conformance tests for #280 slice 3: typed `InlineLet` transform
//! + matching `OperationKind::InlineLet` op.

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

const PLUS_FIVE_SRC: &str =
    "fn plus_five(n :: Int) -> Int { let x := 5; n + x }\n";

fn let_node() -> NodeId { NodeId("n_0.2".into()) }

fn publish_initial(store: &Store) -> (String, String) {
    let s = stage_named(PLUS_FIVE_SRC, "plus_five");
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
fn inline_let_lands_a_typed_op_on_the_branch() {
    let (store, _tmp) = fresh();
    let (sig, from_stage_id) = publish_initial(&store);

    let op_id = store
        .apply_inline_let(DEFAULT_BRANCH, &from_stage_id, &let_node())
        .expect("inline should succeed on well-typed input");

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::InlineLet {
        sig_id: rec_sig,
        from_stage_id: rec_from,
        binding_name,
        let_node: rec_node,
        ..
    } = &rec.op.kind else {
        panic!("expected InlineLet op kind, got {:?}", rec.op.kind);
    };
    assert_eq!(rec_sig, &sig);
    assert_eq!(rec_from, &from_stage_id);
    assert_eq!(binding_name, "x");
    assert_eq!(rec_node, "n_0.2");

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_ne!(head.get(&sig), Some(&from_stage_id));
}

#[test]
fn inline_let_reconstructs_through_get_ast() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial(&store);

    let op_id = store
        .apply_inline_let(DEFAULT_BRANCH, &from_stage_id, &let_node())
        .unwrap();
    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::InlineLet { to_stage_id, .. } = &rec.op.kind else { panic!() };

    let reloaded = store.get_ast(to_stage_id).unwrap();
    let Stage::FnDecl(fd) = reloaded else { panic!() };
    // Body is now `n + 5` — the Let is gone.
    let CExpr::BinOp { lhs, rhs, .. } = fd.body else { panic!() };
    assert!(matches!(*lhs, CExpr::Var { name: ref n } if n == "n"));
    assert!(matches!(*rhs, CExpr::Literal { value: lex_ast::CLit::Int { value: 5 } }));
}

#[test]
fn inline_let_emits_typecheck_attestation() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial(&store);

    let op_id = store
        .apply_inline_let(DEFAULT_BRANCH, &from_stage_id, &let_node())
        .unwrap();

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&op_id).unwrap().unwrap();
    let lex_vcs::OperationKind::InlineLet { to_stage_id, .. } = &rec.op.kind else { panic!() };
    let attlog = store.attestation_log().unwrap();
    let by_stage = attlog.list_for_stage(to_stage_id).unwrap();
    assert!(
        by_stage.iter().any(|a| matches!(a.kind, lex_vcs::AttestationKind::TypeCheck)),
        "TypeCheck attestation should accompany the inlined stage"
    );
}

#[test]
fn inline_let_surface_transform_errors_for_wrong_node() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial(&store);

    // Address a non-Let node.
    let err = store
        .apply_inline_let(DEFAULT_BRANCH, &from_stage_id, &NodeId("n_0.99".into()))
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TransformError(_)),
        "got {err:?}");
}
