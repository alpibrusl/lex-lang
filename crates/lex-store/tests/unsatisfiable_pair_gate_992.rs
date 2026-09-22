//! #992, write-time half: no write path may leave a head naming a
//! `(sig, stage)` pair the store provably cannot hold.
//!
//! #993 stopped `ChangeEffectSig` *producing* the shape and #995/#997 retire
//! it when a publish meets it. Neither stops the shape *landing*: a legacy op
//! (`to_sig_id: None`), a hand-built transition, or a pushed record carrying
//! an old client's `Replace` could still bind the old sig to a stage whose AST
//! is filed under a different sig — and every render of that head then fails
//! with a bare `unknown stage_id`, which is how `lex-web@0.4.0` was born
//! broken.
//!
//! The invariant is "always-valid HEAD". These tests pin it at the three
//! places a head can move or be read:
//!
//! * the local single-op write path (`apply_operation` and friends),
//! * the ref half of `op push` (`advance_branch_head_ff`), and
//! * rendering, which must name the unsatisfiable pair rather than report a
//!   stage id that is, confusingly, present in the store.
//!
//! Each gate is paired with a negative control proving it does not refuse
//! the shapes that are legitimately allowed — above all a stage that is
//! simply *absent* (mid-pull, partial clone), which proves nothing about
//! satisfiability and must keep its existing behaviour.

use std::collections::BTreeSet;

use lex_ast::canonicalize_program;
use lex_store::{Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::parse_source;

const WITH_IO: &str = "fn serve(p :: Str) -> [io] Str { p }\n";
const WITH_FS: &str = "fn serve(p :: Str) -> [fs_read] Str { p }\n";

fn only_fn(src: &str) -> lex_ast::Stage {
    canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(_)))
        .expect("a fn")
}

fn ids(src: &str) -> (String, String) {
    let st = only_fn(src);
    (
        lex_ast::sig_id(&st).expect("sig"),
        lex_ast::stage_id(&st).expect("stage"),
    )
}

fn head_op(s: &Store) -> Option<String> {
    s.get_branch(DEFAULT_BRANCH)
        .unwrap()
        .and_then(|b| b.head_op)
}

fn store() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    (store, tmp)
}

fn add_fn(sig: &str, stage: &str, parent: Option<String>) -> Operation {
    Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stage.into(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        },
        parent.into_iter().collect::<Vec<_>>(),
    )
}

/// Store the AST and land it under its own (correct) sig.
fn land(store: &Store, src: &str) {
    let st = only_fn(src);
    let (sig, stage) = ids(src);
    store.publish(&st).unwrap();
    store
        .apply_operation(
            DEFAULT_BRANCH,
            add_fn(&sig, &stage, head_op(store)),
            StageTransition::Create {
                sig_id: sig,
                stage_id: stage,
            },
        )
        .expect("a correctly filed declaration must land");
}

/// The exact pre-#992 op: a `ChangeEffectSig` with no `to_sig_id`, whose
/// transition rebinds the **old** sig to the **new** stage.
fn legacy_effect_change(store: &Store) -> (Operation, StageTransition) {
    let (sig_io, stage_io) = ids(WITH_IO);
    let (_, stage_fs) = ids(WITH_FS);
    let op = Operation::new(
        OperationKind::ChangeEffectSig {
            sig_id: sig_io.clone(),
            from_stage_id: stage_io.clone(),
            to_stage_id: stage_fs.clone(),
            from_effects: ["io".to_string()].into_iter().collect(),
            to_effects: ["fs_read".to_string()].into_iter().collect(),
            from_budget: None,
            to_budget: None,
            to_sig_id: None,
        },
        head_op(store).into_iter().collect::<Vec<_>>(),
    );
    let transition = StageTransition::Replace {
        sig_id: sig_io,
        from: stage_io,
        to: stage_fs,
    };
    (op, transition)
}

fn is_unsatisfiable(e: &StoreError, sig: &str, stage: &str, owner: &str) -> bool {
    matches!(
        e,
        StoreError::UnsatisfiablePair { sig_id, stage_id, filed_under }
            if sig_id == sig && stage_id == stage && filed_under == owner
    )
}

// ---- the local write path -------------------------------------------------

/// The prod shape, submitted through the single-op write path, is refused
/// with an error that names the pair and where the stage actually lives — and
/// nothing moves.
#[test]
fn apply_refuses_to_bind_a_sig_to_a_stage_filed_under_another() {
    let (store, _tmp) = store();
    let (sig_io, stage_io) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    land(&store, WITH_IO);
    land(&store, WITH_FS);
    let before_op = head_op(&store);
    let before_map = store.branch_head(DEFAULT_BRANCH).unwrap();

    let (op, transition) = legacy_effect_change(&store);
    let err = store
        .apply_operation(DEFAULT_BRANCH, op, transition)
        .expect_err("binding the old sig to a stage filed under the new one must be refused");
    assert!(
        is_unsatisfiable(&err, &sig_io, &stage_fs, &sig_fs),
        "the error must name the pair and the sig that owns the stage: {err:?}"
    );

    assert_eq!(
        head_op(&store),
        before_op,
        "a refused op must not advance the branch"
    );
    let after = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(after, before_map, "and the head map must be untouched");
    assert_eq!(after.get(&sig_io), Some(&stage_io));
}

/// The gated write path funnels through the same check.
#[test]
fn apply_operation_checked_refuses_the_same_shape() {
    let (store, _tmp) = store();
    let (sig_io, _) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    land(&store, WITH_IO);
    land(&store, WITH_FS);

    let (op, transition) = legacy_effect_change(&store);
    let candidate = vec![only_fn(WITH_FS)];
    let err = store
        .apply_operation_checked(DEFAULT_BRANCH, op, transition, &candidate)
        .expect_err("the checked path must refuse it too");
    assert!(
        is_unsatisfiable(&err, &sig_io, &stage_fs, &sig_fs),
        "{err:?}"
    );
}

/// Negative control: the *correct* shape for the same change — the sig moves
/// with the effects — lands, and the resulting head is fully readable.
#[test]
fn the_sig_moving_shape_still_lands() {
    let (store, _tmp) = store();
    let (sig_io, stage_io) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    land(&store, WITH_IO);
    store.publish(&only_fn(WITH_FS)).unwrap();

    let op = Operation::new(
        OperationKind::ChangeEffectSig {
            sig_id: sig_io.clone(),
            from_stage_id: stage_io,
            to_stage_id: stage_fs.clone(),
            from_effects: ["io".to_string()].into_iter().collect(),
            to_effects: ["fs_read".to_string()].into_iter().collect(),
            from_budget: None,
            to_budget: None,
            to_sig_id: Some(sig_fs.clone()),
        },
        head_op(&store).into_iter().collect::<Vec<_>>(),
    );
    let transition = StageTransition::Rename {
        from: sig_io.clone(),
        to: sig_fs.clone(),
        body_stage_id: stage_fs.clone(),
    };
    store
        .apply_operation(DEFAULT_BRANCH, op, transition)
        .expect("the correct shape must land");

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.len(), 1, "{head:?}");
    assert_eq!(head.get(&sig_fs), Some(&stage_fs));
    let pairs: Vec<(String, String)> = head.into_iter().collect();
    assert!(store
        .get_asts_for_sigs_bulk(&pairs)
        .iter()
        .all(|a| a.is_ok()));
}

/// Negative control: a stage the store has never seen under *any* sig is
/// not proof of anything — it may simply not have arrived yet — so the gate
/// must leave it alone, exactly as before. (Refusing it would also break
/// every caller that lands ops ahead of their content.)
#[test]
fn a_merely_absent_stage_is_not_refused() {
    let (store, _tmp) = store();
    let (sig_io, _) = ids(WITH_IO);
    land(&store, WITH_IO);
    let absent = "f".repeat(64);
    let sig_x = "fn::x".to_string();

    store
        .apply_operation(
            DEFAULT_BRANCH,
            add_fn(&sig_x, &absent, head_op(&store)),
            StageTransition::Create {
                sig_id: sig_x.clone(),
                stage_id: absent.clone(),
            },
        )
        .expect("an absent stage is not provably unsatisfiable");
    assert_eq!(
        store.branch_head(DEFAULT_BRANCH).unwrap().get(&sig_x),
        Some(&absent)
    );
    assert!(store
        .branch_head(DEFAULT_BRANCH)
        .unwrap()
        .contains_key(&sig_io));
}

// ---- the ref half of `op push` ---------------------------------------------

/// Persist `op` into the log without touching any branch — how the ops batch
/// of an `op push` lands records on the hub, verbatim from the client.
fn put_record(store: &Store, op: Operation, transition: StageTransition) -> String {
    let rec = lex_vcs::OperationRecord::new(op, transition);
    let id = rec.op_id.clone();
    lex_vcs::OpLog::open(store.root())
        .unwrap()
        .put(&rec)
        .unwrap();
    id
}

/// A pushed history whose tip binds the old sig to the new stage must not
/// become a branch head: the ref advance refuses it and the branch stays put.
#[test]
fn a_push_cannot_advance_a_branch_onto_an_unsatisfiable_head() {
    let (store, _tmp) = store();
    let (sig_io, stage_io) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    store.publish(&only_fn(WITH_IO)).unwrap();
    store.publish(&only_fn(WITH_FS)).unwrap();

    let a = put_record(
        &store,
        add_fn(&sig_io, &stage_io, None),
        StageTransition::Create {
            sig_id: sig_io.clone(),
            stage_id: stage_io.clone(),
        },
    );
    let b = put_record(
        &store,
        add_fn(&sig_fs, &stage_fs, Some(a.clone())),
        StageTransition::Create {
            sig_id: sig_fs.clone(),
            stage_id: stage_fs.clone(),
        },
    );
    store
        .advance_branch_head_ff(DEFAULT_BRANCH, &b)
        .expect("a valid head advances");

    let bad = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig_io.clone(),
            from_stage_id: stage_io.clone(),
            to_stage_id: stage_fs.clone(),
            from_budget: None,
            to_budget: None,
            to_sig_id: None,
        },
        vec![b.clone()],
    );
    let c = put_record(
        &store,
        bad,
        StageTransition::Replace {
            sig_id: sig_io.clone(),
            from: stage_io,
            to: stage_fs.clone(),
        },
    );
    let err = store
        .advance_branch_head_ff(DEFAULT_BRANCH, &c)
        .expect_err("the ref must not move onto a head no store can render");
    assert!(
        is_unsatisfiable(&err, &sig_io, &stage_fs, &sig_fs),
        "{err:?}"
    );
    assert_eq!(
        head_op(&store),
        Some(b),
        "the branch must stay where it was"
    );
}

// ---- rendering --------------------------------------------------------------

/// Existing damage (pulled from a remote, so no write gate ran) cannot be
/// repaired in place, but rendering it must say *what* is wrong: the stage id
/// is present in the store, so a bare `unknown stage_id` is actively
/// misleading. The error names the pair and the sig that owns the stage.
#[test]
fn rendering_a_stranded_head_names_the_unsatisfiable_pair() {
    let (store, _tmp) = store();
    let (sig_io, stage_io) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    store.publish(&only_fn(WITH_IO)).unwrap();
    store.publish(&only_fn(WITH_FS)).unwrap();

    let a = put_record(
        &store,
        add_fn(&sig_io, &stage_io, None),
        StageTransition::Create {
            sig_id: sig_io.clone(),
            stage_id: stage_io.clone(),
        },
    );
    let b = put_record(
        &store,
        add_fn(&sig_fs, &stage_fs, Some(a)),
        StageTransition::Create {
            sig_id: sig_fs.clone(),
            stage_id: stage_fs.clone(),
        },
    );
    let (op, transition) = {
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig_io.clone(),
                from_stage_id: stage_io.clone(),
                to_stage_id: stage_fs.clone(),
                from_budget: None,
                to_budget: None,
                to_sig_id: None,
            },
            vec![b],
        );
        (
            op,
            StageTransition::Replace {
                sig_id: sig_io.clone(),
                from: stage_io,
                to: stage_fs.clone(),
            },
        )
    };
    let c = put_record(&store, op, transition);

    let head = lex_store::render::package_head_at_op(&store, &c).unwrap();
    let err =
        lex_store::render::render_source(&store, &head).expect_err("a stranded head cannot render");
    assert!(
        is_unsatisfiable(&err, &sig_io, &stage_fs, &sig_fs),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains(&sig_io) && msg.contains(&stage_fs) && msg.contains(&sig_fs),
        "the message must name the pair and the owning sig: {msg}"
    );
}

/// Negative control for the render diagnosis: a genuinely absent stage still
/// reports `UnknownStage` — the store really does not have it, and calling
/// that "unsatisfiable" would send the reader after the wrong problem.
#[test]
fn rendering_a_head_with_an_absent_stage_still_says_unknown_stage() {
    let (store, _tmp) = store();
    let absent = "f".repeat(64);
    let a = put_record(
        &store,
        add_fn("fn::x", &absent, None),
        StageTransition::Create {
            sig_id: "fn::x".into(),
            stage_id: absent.clone(),
        },
    );
    let head = lex_store::render::package_head_at_op(&store, &a).unwrap();
    let err = lex_store::render::render_source(&store, &head).expect_err("absent stage");
    assert!(
        matches!(err, StoreError::UnknownStage(ref s) if *s == absent),
        "{err:?}"
    );
}
