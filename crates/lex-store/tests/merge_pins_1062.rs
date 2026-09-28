//! #1062, store-level `commit_merge`: the merge op pins every sig the merge
//! decided — including the ones only dst touched, which used to be left out
//! because they equal dst's own value — while the write-time gate and the
//! TypeCheck attestations still concern only the stages the merge introduces.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn named(src: &str, name: &str) -> lex_ast::Stage {
    lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap())
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == name))
        .expect("fn not found")
}

fn head_op_vec(s: &Store, branch: &str) -> Vec<String> {
    s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect()
}

fn land_add(s: &Store, branch: &str, src: &str, name: &str) -> (String, String) {
    let st = named(src, name);
    let (sig, stg) = (lex_ast::sig_id(&st).unwrap(), lex_ast::stage_id(&st).unwrap());
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
    s.apply_operation_gated(branch, op, StageTransition::Create { sig_id: sig.clone(), stage_id: stg.clone() })
        .expect("gated add must land");
    (sig, stg)
}

fn land_modify(s: &Store, branch: &str, src: &str, name: &str, from: &str) -> String {
    let st = named(src, name);
    let (sig, stg) = (lex_ast::sig_id(&st).unwrap(), lex_ast::stage_id(&st).unwrap());
    s.publish(&st).unwrap();
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.clone(),
            from_stage_id: from.into(),
            to_stage_id: stg.clone(),
            from_budget: None,
            to_budget: None,
            to_sig_id: None,
        },
        head_op_vec(s, branch),
    );
    s.apply_operation_gated(branch, op, StageTransition::Replace { sig_id: sig, from: from.into(), to: stg.clone() })
        .expect("gated modify must land");
    stg
}

#[test]
fn commit_merge_pins_dst_only_changes_but_attests_only_what_it_introduces() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    let (keep_sig, keep_v0) = land_add(&s, DEFAULT_BRANCH, "fn keep(x :: Int) -> Int { x }\n", "keep");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();

    // dst-only change: main modifies `keep`.
    let keep_v1 = land_modify(&s, DEFAULT_BRANCH, "fn keep(x :: Int) -> Int { x + 1 }\n", "keep", &keep_v0);
    // src-only change: feature adds `extra`.
    let (extra_sig, extra_stg) = land_add(&s, "feature", "fn extra(x :: Int) -> Int { x * 2 }\n", "extra");

    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    s.commit_merge(DEFAULT_BRANCH, &report).expect("clean merge must land");

    let merge_op = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
    let rec = lex_vcs::OpLog::open(s.root()).unwrap().get(&merge_op).unwrap().unwrap();
    let StageTransition::Merge { entries } = &rec.produces else { panic!("head is not a merge: {rec:?}") };
    assert_eq!(entries.get(&extra_sig), Some(&Some(extra_stg.clone())), "src's addition: {entries:?}");
    assert_eq!(
        entries.get(&keep_sig),
        Some(&Some(keep_v1.clone())),
        "dst's own change is pinned to dst's value, not left to the replay: {entries:?}"
    );

    // The head is what the resolution says, from a fresh full replay too.
    let want: std::collections::BTreeMap<_, _> =
        [(keep_sig.clone(), keep_v1.clone()), (extra_sig.clone(), extra_stg.clone())].into();
    assert_eq!(s.branch_head(DEFAULT_BRANCH).unwrap(), want);
    assert_eq!(s.sig_map_at_op(&merge_op).unwrap(), want);

    // The merge introduced `extra` only: exactly one merge-attributed
    // TypeCheck::Passed, and none for the stage dst already held.
    let log = s.attestation_log().unwrap();
    let on_merge = |stage: &String| {
        log.list_for_stage(stage)
            .unwrap()
            .into_iter()
            .filter(|a| {
                matches!(a.kind, lex_vcs::AttestationKind::TypeCheck)
                    && matches!(a.result, lex_vcs::AttestationResult::Passed)
                    && a.op_id.as_deref() == Some(merge_op.as_str())
            })
            .count()
    };
    assert_eq!(on_merge(&extra_stg), 1, "introduced stage is attested by the merge");
    assert_eq!(on_merge(&keep_v1), 0, "a pinned dst stage is not re-attested by the merge");
}
