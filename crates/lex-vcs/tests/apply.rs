//! Integration tests for the `lex-vcs` apply pass against a real
//! `lex-store::Store` rooted in a tempdir. Each test publishes one
//! or two real stages from parsed Lex source, applies an
//! `Operation` against them, and verifies the resulting store
//! state plus the persisted `OperationRecord`.

use std::collections::BTreeSet;

use lex_ast::{canonicalize_program, sig_id, Stage};
use lex_store::{StageStatus, Store};
use lex_syntax::parse_source;
use lex_vcs::{
    apply_operation, compute_transition, load_record, ApplyError, Operation, OperationKind,
    StageTransition,
};
use tempfile::TempDir;

fn one_stage(src: &str, name: &str) -> Stage {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    stages
        .into_iter()
        .find(|s| match s {
            Stage::FnDecl(fd) => fd.name == name,
            Stage::TypeDecl(td) => td.name == name,
            _ => false,
        })
        .expect("stage not found")
}

const ADD_V1: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
const ADD_V2: &str = "fn add(x :: Int, y :: Int) -> Int { y + x }\n";
const FACTORIAL: &str =
    "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n";

// ---- AddFunction --------------------------------------------------

#[test]
fn add_function_activates_published_stage_and_persists_record() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let stage = store.publish(&s).unwrap();
    let sig = sig_id(&s).unwrap();

    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stage.clone(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let expected_id = op.op_id();

    let record = apply_operation(&store, op).expect("apply_operation");

    assert_eq!(record.op_id, expected_id);
    assert_eq!(
        record.produces,
        StageTransition::Create {
            sig_id: sig.clone(),
            stage_id: stage.clone(),
        },
    );
    assert_eq!(store.get_status(&stage).unwrap(), StageStatus::Active);
    assert_eq!(store.resolve_sig(&sig).unwrap(), Some(stage.clone()));

    // Persisted under <root>/ops/<OpId>.json.
    let on_disk = load_record(&store, &expected_id).expect("load_record").expect("record exists");
    assert_eq!(on_disk, record);
}

#[test]
fn add_function_against_existing_active_returns_duplicate_add() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let stage = store.publish(&s).unwrap();
    let sig = sig_id(&s).unwrap();
    store.activate(&stage).unwrap();

    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stage.clone(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let expected_id = op.op_id();
    let err = apply_operation(&store, op).expect_err("expected DuplicateAdd");
    match err {
        ApplyError::DuplicateAdd { sig_id: s, existing } => {
            assert_eq!(s, sig);
            assert_eq!(existing, stage);
        }
        other => panic!("expected DuplicateAdd, got {other:?}"),
    }
    // No record persisted on the failure path.
    assert!(load_record(&store, &expected_id).unwrap().is_none());
}

#[test]
fn add_function_against_unpublished_stage_returns_stage_missing() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: "nonexistent::Int->Int".into(),
            stage_id: "fakestage123".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let err = apply_operation(&store, op).expect_err("expected StageMissing");
    assert!(matches!(err, ApplyError::StageMissing(s) if s == "fakestage123"));
}

// ---- ModifyBody ---------------------------------------------------

#[test]
fn modify_body_swaps_active_head() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let v1 = one_stage(ADD_V1, "add");
    let v2 = one_stage(ADD_V2, "add");
    let id1 = store.publish(&v1).unwrap();
    let id2 = store.publish(&v2).unwrap();
    let sig = sig_id(&v1).unwrap();

    // Bring v1 to Active.
    store.activate(&id1).unwrap();
    assert_eq!(store.resolve_sig(&sig).unwrap(), Some(id1.clone()));

    // Now apply ModifyBody to swap to v2.
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.clone(),
            from_stage_id: id1.clone(),
            to_stage_id: id2.clone(),
        },
        [],
    );
    let record = apply_operation(&store, op).expect("apply");

    assert_eq!(
        record.produces,
        StageTransition::Replace {
            sig_id: sig.clone(),
            from: id1.clone(),
            to: id2.clone(),
        },
    );
    assert_eq!(store.resolve_sig(&sig).unwrap(), Some(id2.clone()));
    assert_eq!(store.get_status(&id1).unwrap(), StageStatus::Deprecated);
    assert_eq!(store.get_status(&id2).unwrap(), StageStatus::Active);
}

#[test]
fn modify_body_with_stale_parent_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();

    let v1 = one_stage(ADD_V1, "add");
    let v2 = one_stage(ADD_V2, "add");
    let id1 = store.publish(&v1).unwrap();
    let id2 = store.publish(&v2).unwrap();
    let sig = sig_id(&v1).unwrap();
    store.activate(&id1).unwrap();

    // Submit a ModifyBody whose `from` doesn't match the current
    // active head — simulates a concurrent writer that already
    // moved to v2.
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.clone(),
            from_stage_id: "wrong-parent-id".into(),
            to_stage_id: id2.clone(),
        },
        [],
    );
    let err = apply_operation(&store, op).expect_err("expected StaleParent");
    match err {
        ApplyError::StaleParent { sig_id: s, expected, actual } => {
            assert_eq!(s, sig);
            assert_eq!(expected, "wrong-parent-id");
            assert_eq!(actual, id1);
        }
        other => panic!("expected StaleParent, got {other:?}"),
    }
    // Active head is unchanged.
    assert_eq!(store.resolve_sig(&sig).unwrap(), Some(id1));
}

// ---- ChangeEffectSig ---------------------------------------------

#[test]
fn change_effect_sig_records_old_and_new_effects() {
    // Same store-level effect as ModifyBody (activate new, deprecate
    // old), but the OperationRecord retains the effect-set diff so
    // the future write-time gate (#130) can verify importers.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let v1 = one_stage(ADD_V1, "add");
    let v2 = one_stage(ADD_V2, "add");
    let id1 = store.publish(&v1).unwrap();
    let id2 = store.publish(&v2).unwrap();
    let sig = sig_id(&v1).unwrap();
    store.activate(&id1).unwrap();

    let mut new_effects = BTreeSet::new();
    new_effects.insert("io".to_string());

    let op = Operation::new(
        OperationKind::ChangeEffectSig {
            sig_id: sig.clone(),
            from_stage_id: id1.clone(),
            to_stage_id: id2.clone(),
            from_effects: BTreeSet::new(),
            to_effects: new_effects.clone(),
        },
        [],
    );
    let record = apply_operation(&store, op).expect("apply");
    assert_eq!(
        record.produces,
        StageTransition::Replace { sig_id: sig.clone(), from: id1, to: id2 },
    );

    // The OperationRecord on disk preserves the effect diff.
    let on_disk = load_record(&store, &record.op_id).unwrap().unwrap();
    match on_disk.op.kind {
        OperationKind::ChangeEffectSig { from_effects, to_effects, .. } => {
            assert!(from_effects.is_empty());
            assert_eq!(to_effects, new_effects);
        }
        other => panic!("expected ChangeEffectSig, got {other:?}"),
    }
}

// ---- RemoveFunction ----------------------------------------------

#[test]
fn remove_function_tombstones_the_active_head() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let stage = store.publish(&s).unwrap();
    let sig = sig_id(&s).unwrap();
    store.activate(&stage).unwrap();

    let op = Operation::new(
        OperationKind::RemoveFunction {
            sig_id: sig.clone(),
            last_stage_id: stage.clone(),
        },
        [],
    );
    let record = apply_operation(&store, op).expect("apply");
    assert_eq!(
        record.produces,
        StageTransition::Remove { sig_id: sig.clone(), last: stage.clone() },
    );
    assert_eq!(store.get_status(&stage).unwrap(), StageStatus::Tombstone);
    // After tombstoning, resolve_sig has no Active head.
    assert_eq!(store.resolve_sig(&sig).unwrap(), None);
}

// ---- AddImport / RemoveImport ------------------------------------

#[test]
fn add_import_persists_record_without_touching_stages() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let op = Operation::new(
        OperationKind::AddImport {
            in_file: "src/main.lex".into(),
            module: "std.io".into(),
        },
        [],
    );
    let record = apply_operation(&store, op).expect("apply");
    assert_eq!(record.produces, StageTransition::ImportOnly);
    assert!(load_record(&store, &record.op_id).unwrap().is_some());
    // No stages directory should have been created.
    let stages_dir = tmp.path().join("stages");
    let entries: Vec<_> = std::fs::read_dir(&stages_dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(entries.is_empty(), "no stage directories should be created");
}

// ---- RenameSymbol (deferred) -------------------------------------

#[test]
fn rename_symbol_returns_not_yet_implemented() {
    // Documented limitation: the op enum carries a single
    // `body_stage_id`, but lex-store's StageId hash includes the
    // symbol name, so a rename involves two stages with different
    // ids. Tightening the op enum is a follow-up; until then the
    // apply pass surfaces a clean error rather than silently
    // corrupting state.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let op = Operation::new(
        OperationKind::RenameSymbol {
            from: "parse::Str->Int".into(),
            to: "parse_int::Str->Int".into(),
            body_stage_id: "abc123".into(),
        },
        [],
    );
    let err = apply_operation(&store, op).expect_err("expected NotYetImplemented");
    assert!(matches!(err, ApplyError::NotYetImplemented("RenameSymbol")));
}

// ---- compute_transition (preview / dry-run) ----------------------

#[test]
fn compute_transition_does_not_mutate_the_store() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = one_stage(FACTORIAL, "factorial");
    let stage = store.publish(&s).unwrap();
    let sig = sig_id(&s).unwrap();

    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stage.clone(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let preview = compute_transition(&store, &op).expect("preview");
    assert_eq!(
        preview,
        StageTransition::Create { sig_id: sig.clone(), stage_id: stage.clone() },
    );
    // The published stage is still Draft, not Active.
    assert_eq!(store.get_status(&stage).unwrap(), StageStatus::Draft);
    // No op record should be persisted by a preview.
    assert!(load_record(&store, &op.op_id()).unwrap().is_none());
}
