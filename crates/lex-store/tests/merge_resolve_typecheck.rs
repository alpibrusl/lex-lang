//! #834: the merge session's `resolve` re-type-checks resolutions.
//!
//! Before this, `MergeSession::resolve` accepted every structurally
//! valid resolution and the type failure only surfaced at commit —
//! turning the documented "submit N resolutions, see which broke,
//! retry" loop into "commit, fail, restart the whole session". The
//! store-backed `MergeResolutionChecker` lets `resolve_checked`
//! type-check the projected program per resolution: a resolution whose
//! composed program doesn't type-check is rejected on submission and
//! not recorded.

use lex_store::{
    MergeResolutionChecker, Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH,
};
use lex_vcs::{MergeSession, OpLog, Resolution, ResolutionRejection};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn named(src: &str, name: &str) -> lex_ast::Stage {
    lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap())
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == name))
        .expect("fn not found")
}

fn head_op_vec(s: &Store, branch: &str) -> Vec<String> {
    s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect()
}

/// Publish `name` from `src` and land it on `branch` (single-parent gate).
fn land_add(s: &Store, branch: &str, src: &str, name: &str) -> (String, String) {
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
        head_op_vec(s, branch),
    );
    let t = StageTransition::Create { sig_id: sig.clone(), stage_id: stg.clone() };
    s.apply_operation_gated(branch, op, t).expect("gated add must land");
    (sig, stg)
}

/// Replace `name`'s body on `branch` with `src` (single-parent gate).
fn land_modify(s: &Store, branch: &str, src: &str, name: &str) -> String {
    let st = named(src, name);
    let sig = lex_ast::sig_id(&st).unwrap();
    let new_stg = lex_ast::stage_id(&st).unwrap();
    let from = s.branch_head(branch).unwrap().get(&sig).cloned().expect("sig on branch");
    s.publish(&st).unwrap();
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.clone(),
            from_stage_id: from.clone(),
            to_stage_id: new_stg.clone(),
            from_budget: None,
            to_budget: None,
            to_sig_id: None,
        },
        head_op_vec(s, branch),
    );
    let t = StageTransition::Replace { sig_id: sig.clone(), from, to: new_stg.clone() };
    s.apply_operation_gated(branch, op, t).expect("gated modify must land");
    new_stg
}

fn session_for(s: &Store, src_branch: &str, dst_branch: &str) -> MergeSession {
    let log = OpLog::open(s.root()).unwrap();
    let src = s.get_branch(src_branch).unwrap().and_then(|b| b.head_op);
    let dst = s.get_branch(dst_branch).unwrap().and_then(|b| b.head_op);
    MergeSession::start("ms", &log, src.as_ref(), dst.as_ref()).unwrap()
}

const HELPER: &str = "fn helper(x :: Int) -> Int { x }\n";

#[test]
fn resolve_checked_rejects_a_pick_that_calls_a_dropped_helper() {
    // dst: { helper, caller } where caller calls helper.
    // feature: helper removed AND caller rewritten to call helper2
    //   (a fn that doesn't exist on dst) — a divergent caller body.
    // Merging, the conflict on `caller` offers TakeTheirs (feature's
    // caller, which calls the absent helper2) — that must NOT compose
    // against dst's head and must be rejected at resolve time.
    let (s, _tmp) = fresh();
    land_add(&s, DEFAULT_BRANCH, HELPER, "helper");
    let caller_dst = format!("{HELPER}fn caller(x :: Int) -> Int {{ helper(x) }}\n");
    land_add(&s, DEFAULT_BRANCH, &caller_dst, "caller");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();

    // On feature, rewrite caller to reference an undefined identifier
    // (`ghost`) so the theirs side is self-broken: it composes with
    // nothing. We publish it via the *ungated* apply below, precisely
    // because the single-parent gate would reject a self-broken body.
    let caller_ghost = format!("{HELPER}fn caller(x :: Int) -> Int {{ ghost(x) }}\n");
    // publish theirs body directly (bypass the single-parent gate,
    // which would itself reject the self-broken body) so the merge
    // sees a genuine divergent-but-unchecked theirs stage.
    let theirs_stage = {
        let st = named(&caller_ghost, "caller");
        let sig = lex_ast::sig_id(&st).unwrap();
        let new_stg = lex_ast::stage_id(&st).unwrap();
        let from = s.branch_head("feature").unwrap().get(&sig).cloned().unwrap();
        s.publish(&st).unwrap();
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.clone(),
                from_stage_id: from.clone(),
                to_stage_id: new_stg.clone(),
                from_budget: None,
                to_budget: None,
                to_sig_id: None,
            },
            head_op_vec(&s, "feature"),
        );
        let t = StageTransition::Replace { sig_id: sig, from, to: new_stg.clone() };
        // ungated apply: we intentionally want a divergent theirs the
        // merge must catch, not the single-parent gate.
        s.apply_operation("feature", op, t).unwrap();
        new_stg
    };

    // dst also modifies caller (a harmless reformat) so the merge is a
    // real ModifyModify conflict rather than a one-sided take.
    land_modify(&s, DEFAULT_BRANCH, &format!("{HELPER}fn caller(x :: Int) -> Int {{ helper(helper(x)) }}\n"), "caller");

    let mut session = session_for(&s, "feature", DEFAULT_BRANCH);
    let conflicts = session.remaining_conflicts();
    // sig_ids are content hashes, not names; the caller conflict is the
    // one whose `theirs` is the ghost-calling body we published.
    let caller = conflicts
        .iter()
        .find(|c| c.theirs.as_deref() == Some(theirs_stage.as_str()))
        .expect("expected the caller ModifyModify conflict");
    let caller_conflict = caller.conflict_id.clone();
    drop(conflicts);

    let checker = MergeResolutionChecker::new(&s, DEFAULT_BRANCH);
    // TakeTheirs picks the ghost-calling body → composed program has
    // an unknown identifier → rejected at resolve time.
    let verdicts = session.resolve_checked(
        vec![(caller_conflict.clone(), Resolution::TakeTheirs)],
        &checker,
    );
    assert_eq!(verdicts.len(), 1);
    assert!(!verdicts[0].accepted, "theirs body calls an undefined fn; must be rejected");
    assert!(matches!(verdicts[0].rejection, Some(ResolutionRejection::TypeError { .. })),
        "got {:?}", verdicts[0].rejection);

    // TakeOurs (dst's valid caller) composes and is accepted.
    let verdicts = session.resolve_checked(
        vec![(caller_conflict, Resolution::TakeOurs)],
        &checker,
    );
    assert!(verdicts[0].accepted, "dst's caller composes; got {:?}", verdicts[0].rejection);
    assert!(session.remaining_conflicts().is_empty(), "conflict resolved");
}

#[test]
fn resolve_checked_accepts_a_composing_pick() {
    // Both sides change `f`'s body to a self-contained valid body;
    // TakeTheirs composes and is accepted.
    let (s, _tmp) = fresh();
    land_add(&s, DEFAULT_BRANCH, "fn f(x :: Int) -> Int { x }\n", "f");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let theirs = land_modify(&s, "feature", "fn f(x :: Int) -> Int { x + 1 }\n", "f");
    land_modify(&s, DEFAULT_BRANCH, "fn f(x :: Int) -> Int { x + 2 }\n", "f");

    let mut session = session_for(&s, "feature", DEFAULT_BRANCH);
    let cid = session.remaining_conflicts()[0].conflict_id.clone();
    let checker = MergeResolutionChecker::new(&s, DEFAULT_BRANCH);
    let verdicts = session.resolve_checked(vec![(cid, Resolution::TakeTheirs)], &checker);
    assert!(verdicts[0].accepted, "a self-contained theirs body must compose; got {:?}", verdicts[0].rejection);
    // The projection would set f to theirs.
    let _ = theirs;
}
