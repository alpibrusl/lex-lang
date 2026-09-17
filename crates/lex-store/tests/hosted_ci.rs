//! #93 hosted CI runner: on head advance the hub independently re-runs
//! the type-check gate and records a `lex-hub-ci`-produced TypeCheck
//! attestation, so `require-attestation` gates are backed by a trusted
//! server-side producer rather than a client-attached attestation.

use lex_ast::canonicalize_program;
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    canonicalize_program(&parse_source(src).expect("parse"))
}

#[test]
fn hosted_ci_records_a_lex_hub_ci_typecheck_on_the_head() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();

    let stages = parse("fn add(x :: Int, y :: Int) -> Int { x + y }\n");
    let new_fns: std::collections::BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let ef: std::collections::BTreeMap<String, lex_ast::FnDecl> = Default::default();
    let et: std::collections::BTreeMap<String, lex_ast::TypeDecl> = Default::default();
    let diff = lex_vcs::compute_diff_with_types(&ef, &new_fns, &et, &et, true);
    let imports = lex_vcs::ImportMap::new();
    let outcome = s
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &imports, true)
        .unwrap();
    let head = outcome.head_op.expect("head_op");

    // The hosted runner verifies the new head and attests it.
    let verdict = s
        .verify_head_and_attest(DEFAULT_BRANCH, None, &head)
        .unwrap();
    assert!(verdict.passed, "head must type-check: {verdict:?}");
    assert!(verdict.attested_stages >= 1, "should attest the head's stage");

    // That stage now carries a TypeCheck attestation from `lex-hub-ci`.
    let head_map = s.branch_head(DEFAULT_BRANCH).unwrap();
    let stage_id = head_map.values().next().expect("a head stage");
    let log = s.attestation_log().unwrap();
    let atts = log.list_for_stage(stage_id).unwrap();
    let hub = atts.iter().find(|a| {
        matches!(a.kind, lex_vcs::AttestationKind::TypeCheck) && a.produced_by.tool == "lex-hub-ci"
    });
    assert!(hub.is_some(), "expected a lex-hub-ci TypeCheck, got: {atts:?}");
    assert!(matches!(hub.unwrap().result, lex_vcs::AttestationResult::Passed));

    // Idempotent: re-verifying the same head adds no new attestations.
    let before = log.list_for_stage(stage_id).unwrap().len();
    s.verify_head_and_attest(DEFAULT_BRANCH, None, &head).unwrap();
    assert_eq!(s.attestation_log().unwrap().list_for_stage(stage_id).unwrap().len(), before);
}
