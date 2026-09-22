//! #868: replay's parent-program reconstruction must not fail hard when the
//! reconstructed head names a stage the store can no longer load (a GC'd,
//! superseded or never-persisted intermediate).
//!
//! Before the fix, `replay_request` bulk-loaded every head stage with
//! `collect::<Result<_, _>>()?`, so a single missing stage anywhere in the
//! parent state turned the whole replay into `unknown stage_id <hash>`. Now
//! unloadable *context* is dropped from the parent program and reported in
//! `ReplayRequest::skipped` — but an unloadable *target* is still a clear
//! error, because there is nothing to replay against.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;
use std::path::Path;

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
            in_file: None,
        },
        s.get_branch(DEFAULT_BRANCH)
            .unwrap()
            .and_then(|b| b.head_op)
            .into_iter()
            .collect::<Vec<_>>(),
    );
    let t = StageTransition::Create {
        sig_id: sig.clone(),
        stage_id: stg.clone(),
    };
    let op_id = s
        .apply_operation_gated(DEFAULT_BRANCH, op, t)
        .expect("gated add");
    (op_id, sig, stg)
}

/// Make a stage unloadable the way GC / partial retention would: its AST
/// file is gone from disk while the op-log still names it.
fn drop_stage_ast(root: &Path, sig: &str, stage: &str) {
    let p = root
        .join("stages")
        .join(sig)
        .join("implementations")
        .join(format!("{stage}.ast.json"));
    std::fs::remove_file(&p).unwrap_or_else(|e| panic!("removing {}: {e}", p.display()));
}

fn drop_stage_metadata(root: &Path, sig: &str, stage: &str) {
    let p = root
        .join("stages")
        .join(sig)
        .join("implementations")
        .join(format!("{stage}.metadata.json"));
    std::fs::remove_file(&p).unwrap_or_else(|e| panic!("removing {}: {e}", p.display()));
}

const HELPER: &str = "fn helper(x :: Int) -> Int { x }\n";
const OTHER: &str = "fn other(x :: Int) -> Int { x + 1 }\n";
const G: &str = "fn helper(x :: Int) -> Int { x }\nfn g(x :: Int) -> Int { helper(x) }\n";

/// helper, other, then g (which calls helper). Returns g's op + the sigs/stages.
struct Fixture {
    store: Store,
    tmp: tempfile::TempDir,
    helper: (String, String),
    other: (String, String),
    g_op: String,
    g: (String, String),
}

fn fixture() -> Fixture {
    let (store, tmp) = fresh();
    let (_, hs, hst) = land_add(&store, HELPER, "helper");
    let (_, os, ost) = land_add(&store, OTHER, "other");
    let (g_op, gs, gst) = land_add(&store, G, "g");
    Fixture {
        store,
        tmp,
        helper: (hs, hst),
        other: (os, ost),
        g_op,
        g: (gs, gst),
    }
}

#[test]
fn an_unloadable_parent_stage_is_skipped_and_reported_not_fatal() {
    let f = fixture();
    drop_stage_ast(f.tmp.path(), &f.helper.0, &f.helper.1);

    let req = f
        .store
        .replay_request(&f.g_op)
        .expect("an unloadable *context* stage must not abort the replay (#868)");

    assert_eq!(
        req.skipped.len(),
        1,
        "exactly the missing stage is skipped: {:?}",
        req.skipped
    );
    let sk = &req.skipped[0];
    assert_eq!(sk.sig_id, f.helper.0);
    assert_eq!(sk.stage_id, f.helper.1);
    assert!(!sk.reason.is_empty(), "the skip must say why");
    // Only the AST is gone; the name survives in the stage metadata.
    assert_eq!(sk.name.as_deref(), Some("helper"));
    // g calls helper — the regenerator is looking at a dangling reference.
    assert!(
        sk.called_by_target,
        "g calls helper; the skip must say so: {sk:?}"
    );

    // What *is* loadable is still there; what isn't is not.
    assert!(
        req.parent_program.contains("fn other"),
        "parent: {}",
        req.parent_program
    );
    assert!(
        !req.parent_program.contains("fn helper"),
        "parent: {}",
        req.parent_program
    );
    assert_eq!(req.target_name.as_deref(), Some("g"));

    // Additive JSON field.
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["skipped"][0]["stage_id"], f.helper.1.as_str());
    assert_eq!(v["skipped"][0]["called_by_target"], true);
}

#[test]
fn a_skipped_stage_the_target_does_not_call_is_not_flagged() {
    let f = fixture();
    drop_stage_ast(f.tmp.path(), &f.other.0, &f.other.1);

    let req = f.store.replay_request(&f.g_op).expect("skip, not fail");
    assert_eq!(req.skipped.len(), 1);
    assert_eq!(req.skipped[0].sig_id, f.other.0);
    assert_eq!(req.skipped[0].name.as_deref(), Some("other"));
    assert!(
        !req.skipped[0].called_by_target,
        "g never calls other: {:?}",
        req.skipped[0]
    );
    assert!(
        req.parent_program.contains("fn helper"),
        "parent: {}",
        req.parent_program
    );
    assert!(
        !req.parent_program.contains("fn other"),
        "parent: {}",
        req.parent_program
    );
}

#[test]
fn a_skip_with_no_recoverable_name_still_replays() {
    let f = fixture();
    drop_stage_ast(f.tmp.path(), &f.other.0, &f.other.1);
    drop_stage_metadata(f.tmp.path(), &f.other.0, &f.other.1);

    let req = f.store.replay_request(&f.g_op).expect("skip, not fail");
    assert_eq!(req.skipped.len(), 1);
    assert_eq!(req.skipped[0].name, None);
    assert!(!req.skipped[0].called_by_target);
}

/// Negative control: the op's own target is the one thing replay cannot do
/// without. It must still fail — and say which stage and why — rather than
/// degrade into a replay of nothing.
#[test]
fn an_unloadable_target_stage_is_still_a_clear_error() {
    let f = fixture();
    drop_stage_ast(f.tmp.path(), &f.g.0, &f.g.1);

    let err = f
        .store
        .replay_request(&f.g_op)
        .expect_err("cannot replay a target that cannot be loaded");
    let msg = err.to_string();
    assert!(
        matches!(err, lex_store::StoreError::ReplayTargetUnloadable { .. }),
        "expected ReplayTargetUnloadable, got {err:?}"
    );
    assert!(
        msg.contains(&f.g.1),
        "error must name the target stage: {msg}"
    );
    assert!(msg.contains(&f.g_op), "error must name the op: {msg}");
}

/// A clean store is unchanged: nothing skipped, and the JSON shape carries no
/// new key (backward compatible for existing consumers).
#[test]
fn a_clean_store_skips_nothing_and_serializes_as_before() {
    let f = fixture();
    let req = f.store.replay_request(&f.g_op).unwrap();
    assert!(req.skipped.is_empty());
    assert!(req.parent_program.contains("fn helper") && req.parent_program.contains("fn other"));
    let v = serde_json::to_value(&req).unwrap();
    assert!(
        v.get("skipped").is_none(),
        "no `skipped` key when nothing was skipped: {v}"
    );
}

/// Both reconstruction paths (de-mangled, and the raw fallback used for
/// multi-module heads) skip-and-report rather than erroring — while their
/// strict counterparts keep failing, for callers that need a complete head.
#[test]
fn both_reconstruction_paths_skip_and_report() {
    let f = fixture();
    drop_stage_ast(f.tmp.path(), &f.helper.0, &f.helper.1);

    for (label, got) in [
        (
            "demangled",
            f.store.demangled_program_at_op_skipping(&f.g_op).unwrap(),
        ),
        (
            "raw",
            f.store.program_stages_at_op_skipping(&f.g_op).unwrap(),
        ),
    ] {
        let names: Vec<&str> = got
            .stages
            .iter()
            .filter_map(|s| match s {
                lex_ast::Stage::FnDecl(fd) => Some(fd.name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"g") && names.contains(&"other"),
            "{label}: {names:?}"
        );
        assert!(!names.contains(&"helper"), "{label}: {names:?}");
        assert_eq!(got.skipped.len(), 1, "{label}: {:?}", got.skipped);
        assert_eq!(got.skipped[0].stage_id, f.helper.1, "{label}");
    }

    assert!(
        f.store.demangled_program_at_op(&f.g_op).is_err(),
        "strict path stays strict"
    );
    assert!(
        f.store.program_stages_at_op(&f.g_op).is_err(),
        "strict path stays strict"
    );
}

/// Only a genuinely *absent* stage may be skipped. A context stage that is
/// present but corrupt is store corruption: replay must fail and name the
/// stage, not quietly report "context incomplete".
#[test]
fn a_corrupt_context_stage_fails_replay_instead_of_being_skipped() {
    let f = fixture();
    let p = f
        .tmp
        .path()
        .join("stages")
        .join(&f.helper.0)
        .join("implementations")
        .join(format!("{}.ast.json", f.helper.1));
    std::fs::write(&p, b"{ this is not json").unwrap();

    let err = f
        .store
        .replay_request(&f.g_op)
        .expect_err("a corrupt context stage must not be skipped");
    assert!(
        matches!(err, lex_store::StoreError::StageUnreadable { ref stage_id, .. } if *stage_id == f.helper.1),
        "expected StageUnreadable for the corrupt stage, got {err:?}"
    );
    assert!(
        err.to_string().contains(&f.helper.1),
        "error must name the stage: {err}"
    );

    // Both lenient reconstruction paths refuse it too.
    assert!(f.store.demangled_program_at_op_skipping(&f.g_op).is_err());
    assert!(f.store.program_stages_at_op_skipping(&f.g_op).is_err());
}
