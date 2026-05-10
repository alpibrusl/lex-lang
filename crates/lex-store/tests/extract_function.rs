//! Conformance tests for #280 slice 4: typed `ExtractFunction`
//! transform — emits AddFunction + ModifyBody ops linked by a
//! shared synthetic Intent.

use lex_ast::{
    canonicalize_program, sig_id, stage_id, CExpr, ExtractFnSpec,
    NodeId, Param, Stage, TypeExpr,
};
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

const COMBINE_SRC: &str = "fn combine(n :: Int, m :: Int) -> Int { (n * 2) + m }\n";

fn publish_initial(store: &Store) -> (String, String) {
    let s = stage_named(COMBINE_SRC, "combine");
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

fn double_n_spec() -> ExtractFnSpec {
    ExtractFnSpec {
        name: "double_n".into(),
        type_params: Vec::new(),
        params: vec![Param {
            name: "n".into(),
            ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
        }],
        effects: Vec::new(),
        return_type: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
    }
}

#[test]
fn extract_function_emits_two_linked_ops() {
    let (store, _tmp) = fresh();
    let (source_sig, from_stage_id) = publish_initial(&store);

    // The body is `(n * 2) + m`. Its lhs at NodeId `n_0.3.0` is
    // the `n * 2` sub-expression. (2 params + return + body slot
    // = body at child 3; lhs of the BinOp at 3.0.)
    let (add_op, modify_op) = store
        .apply_extract_function(
            DEFAULT_BRANCH,
            &from_stage_id,
            &NodeId("n_0.3.0".into()),
            double_n_spec(),
        )
        .expect("extract should succeed on well-typed input");

    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let add_rec = log.get(&add_op).unwrap().unwrap();
    let mod_rec = log.get(&modify_op).unwrap().unwrap();

    // Add op carries an AddFunction kind for the new fn.
    let lex_vcs::OperationKind::AddFunction { sig_id: add_sig, .. } = &add_rec.op.kind else {
        panic!("expected AddFunction, got {:?}", add_rec.op.kind);
    };
    assert_ne!(add_sig, &source_sig, "new fn has its own sig");

    // Modify op carries a ModifyBody kind on the source sig.
    let lex_vcs::OperationKind::ModifyBody { sig_id: mod_sig, .. } = &mod_rec.op.kind else {
        panic!("expected ModifyBody, got {:?}", mod_rec.op.kind);
    };
    assert_eq!(mod_sig, &source_sig);

    // Both ops share the same intent_id.
    let intent_a = add_rec.op.intent_id.as_deref().expect("add op has intent");
    let intent_b = mod_rec.op.intent_id.as_deref().expect("modify op has intent");
    assert_eq!(intent_a, intent_b, "both ops share the synthetic Intent");
}

#[test]
fn extract_function_intent_carries_structured_prompt() {
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial(&store);

    let (add_op, _modify_op) = store
        .apply_extract_function(
            DEFAULT_BRANCH,
            &from_stage_id,
            &NodeId("n_0.3.0".into()),
            double_n_spec(),
        )
        .unwrap();
    let log = lex_vcs::OpLog::open(store.root()).unwrap();
    let rec = log.get(&add_op).unwrap().unwrap();
    let intent_id = rec.op.intent_id.as_deref().unwrap().to_string();

    let intent_log = lex_vcs::IntentLog::open(store.root()).unwrap();
    let intent = intent_log.get(&intent_id).unwrap().unwrap();
    assert!(intent.prompt.starts_with("[lex.transform.extract_function]"));
    assert!(intent.prompt.contains("new_fn=double_n"));
    assert!(intent.prompt.contains("expr_node=n_0.3.0"));
}

#[test]
fn extract_function_branch_state_is_consistent_after_both_ops() {
    let (store, _tmp) = fresh();
    let (source_sig, _from_stage_id) = publish_initial(&store);

    let (_add, _modify) = store
        .apply_extract_function(
            DEFAULT_BRANCH,
            &_from_stage_id,
            &NodeId("n_0.3.0".into()),
            double_n_spec(),
        )
        .unwrap();

    // After both ops the branch carries: source sig (modified) +
    // new fn sig (added).
    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.len(), 2, "two sigs on branch head, got {head:?}");
    assert!(head.contains_key(&source_sig));
    assert!(head.keys().any(|k| k != &source_sig), "new sig present");

    // Reload the source's modified body — its lhs is now a Call.
    let modified_source_id = head.get(&source_sig).unwrap();
    let ast = store.get_ast(modified_source_id).unwrap();
    let Stage::FnDecl(fd) = ast else { panic!() };
    let CExpr::BinOp { lhs, .. } = fd.body else { panic!() };
    let CExpr::Call { callee, args } = *lhs else { panic!() };
    assert!(matches!(*callee, CExpr::Var { name: ref n } if n == "double_n"));
    assert_eq!(args.len(), 1);
}

#[test]
fn extract_function_surfaces_transform_errors() {
    // Targeting a node that doesn't address an expression.
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial(&store);
    let err = store
        .apply_extract_function(
            DEFAULT_BRANCH,
            &from_stage_id,
            &NodeId("n_0.99".into()),
            double_n_spec(),
        )
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TransformError(_)),
        "got {err:?}");
}

#[test]
fn extract_function_surfaces_param_mismatch() {
    // Spec has an extra param not free in the extracted expr.
    let (store, _tmp) = fresh();
    let (_sig, from_stage_id) = publish_initial(&store);
    let mut spec = double_n_spec();
    spec.params.push(Param {
        name: "z".into(),
        ty: TypeExpr::Named { name: "Int".into(), args: Vec::new() },
    });
    let err = store
        .apply_extract_function(
            DEFAULT_BRANCH,
            &from_stage_id,
            &NodeId("n_0.3.0".into()),
            spec,
        )
        .unwrap_err();
    assert!(matches!(err, lex_store::StoreError::TransformError(
        lex_ast::TransformError::ExtractFnRefused { .. }
    )), "got {err:?}");
}
