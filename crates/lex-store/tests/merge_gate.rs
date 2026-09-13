//! #833: the write-time gate on the merge path.
//!
//! `commit_merge` lands a `Merge` op through
//! `Store::apply_merge_op_gated`, which type-checks the real
//! post-merge head (the op-DAG replay of both parents, not the
//! delta-only `entries`) and rolls the head back if it doesn't
//! compose.
//!
//! With every write gated, each branch is always individually valid,
//! and the auto-merge engine tends to keep a still-needed sig rather
//! than drop it — so the reachable broken-merge is an agent-supplied
//! `Custom` resolution (or an adversarial op) that drops a sig the
//! rest of the head still calls. A gate reasoning from `entries`
//! alone would mis-judge merges whose replay re-surfaces sigs from
//! the second parent; this one replays the actual head.

use lex_store::{Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH};
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

/// Publish `name` from `src` and land it on `branch` through the
/// single-parent gated apply.
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
        },
        head_op_vec(s, branch),
    );
    let t = StageTransition::Create { sig_id: sig.clone(), stage_id: stg.clone() };
    s.apply_operation_gated(branch, op, t).expect("gated add must land");
    (sig, stg)
}

const HELPER: &str = "fn helper(x :: Int) -> Int { x }\n";

#[test]
fn merge_op_that_drops_a_still_referenced_fn_is_refused_and_head_rolls_back() {
    // dst has { helper, caller } with caller calling helper. A merge
    // transition dropping helper (entries { helper: None }) — the
    // shape a Custom resolution or an adversarial second branch
    // produces over the commit path — leaves the post-merge head
    // { caller } calling an absent helper. apply_merge_op_gated must
    // refuse it and roll the head back.
    use std::collections::BTreeMap;
    let (s, _tmp) = fresh();
    let (helper_sig, _) = land_add(&s, DEFAULT_BRANCH, HELPER, "helper");
    let _ = land_add(&s, DEFAULT_BRANCH, &format!("{HELPER}fn caller(x :: Int) -> Int {{ helper(x) }}\n"), "caller");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();

    let dst_head_before = head_op_vec(&s, DEFAULT_BRANCH);
    let d = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
    let src = s.get_branch("feature").unwrap().unwrap().head_op.unwrap();

    let mut entries: BTreeMap<String, Option<String>> = BTreeMap::new();
    entries.insert(helper_sig.clone(), None); // drop helper
    let op = Operation::new(OperationKind::Merge { resolved: 1 }, [src, d]);
    let t = StageTransition::Merge { entries };

    let err = s.apply_merge_op_gated(DEFAULT_BRANCH, op, t).expect_err("must be refused");
    assert!(matches!(err, StoreError::TypeError(_)), "got {err:?}");

    assert_eq!(head_op_vec(&s, DEFAULT_BRANCH), dst_head_before);
    assert!(s.branch_head(DEFAULT_BRANCH).unwrap().contains_key(&helper_sig),
        "helper must still be on dst after the rollback");
}

#[test]
fn a_composing_merge_still_lands() {
    // The gate is a gate, not a wall: an unambiguous conflict-free
    // merge (feature adds an independent `extra`, main keeps helper)
    // composes and must land.
    let (s, _tmp) = fresh();
    let (helper_sig, _) = land_add(&s, DEFAULT_BRANCH, HELPER, "helper");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let (extra_sig, extra_stg) =
        land_add(&s, "feature", &format!("{HELPER}fn extra(x :: Int) -> Int {{ x + 1 }}\n"), "extra");

    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    s.commit_merge(DEFAULT_BRANCH, &report).expect("a composing merge must land");

    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert!(head.contains_key(&helper_sig));
    assert_eq!(head.get(&extra_sig), Some(&extra_stg));
    assert_eq!(s.branch_log(DEFAULT_BRANCH).unwrap().len(), 1);
}

#[test]
fn candidate_program_for_replays_a_single_parent_transition_over_the_head() {
    // apply_operation_gated (patch) uses candidate_program_for, which
    // is exact for single-parent transitions.
    let (s, _tmp) = fresh();
    let (helper_sig, helper_stg) = land_add(&s, DEFAULT_BRANCH, HELPER, "helper");
    let st_caller = named(&format!("{HELPER}fn caller(x :: Int) -> Int {{ helper(x) }}\n"), "caller");
    let caller_sig = lex_ast::sig_id(&st_caller).unwrap();
    let caller_stg = lex_ast::stage_id(&st_caller).unwrap();
    s.publish(&st_caller).unwrap();

    let t = StageTransition::Create { sig_id: caller_sig, stage_id: caller_stg };
    let prog = s.candidate_program_for(DEFAULT_BRANCH, &t).unwrap();
    let mut names: Vec<&str> = prog.iter().filter_map(|st| match st {
        lex_ast::Stage::FnDecl(fd) => Some(fd.name.as_str()),
        _ => None,
    }).collect();
    names.sort();
    assert_eq!(names, vec!["caller", "helper"]);

    let t = StageTransition::Remove { sig_id: helper_sig, last: helper_stg };
    assert!(s.candidate_program_for(DEFAULT_BRANCH, &t).unwrap().is_empty());
}
