//! #956: agent-refined acceptance. A free-form issue takes a proposed typed
//! acceptance; a human's `Review` on the proposal decides it; the issue's
//! effective acceptance is its latest approved proposal and its id never
//! changes. Typed issues are not refinable.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use lex_ast::canonicalize_program;
use lex_store::issues::{
    check_refinable, effective_acceptance, evaluate_static, proposal_status,
    record_proposal_review, with_effective_acceptance, IssueEvaluation, ProposalStatus,
};
use lex_store::{Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_vcs::{Acceptance, AcceptanceProposal, ApiChangeKind, ApiEntry, Issue, IssueLog};

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

fn publish(store: &Store, src: &str) -> String {
    let stages = canonicalize_program(&parse_source(src).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&BTreeMap::new(), &new, &et, &et, true);
    store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &lex_vcs::ImportMap::new(), true)
        .expect("publish")
        .head_op
        .expect("head op")
}

fn clamp_delta() -> Acceptance {
    Acceptance::TypedDelta {
        api: vec![ApiEntry {
            name: "clamp".into(),
            signature: "(x :: Int, lo :: Int, hi :: Int) -> Int".into(),
            kind: ApiChangeKind::Added,
        }],
        examples: vec!["clamp(5, 0, 3) => 3".into()],
    }
}

fn free_form(store: &Store) -> Issue {
    let issue = Issue::with_timestamp("clamp", "", Acceptance::FreeForm {}, None, BTreeSet::new(), None, 1);
    IssueLog::open(store.root()).unwrap().put(&issue).unwrap();
    issue
}

fn propose(store: &Store, issue: &Issue, acc: Acceptance) -> AcceptanceProposal {
    let p = AcceptanceProposal::new(issue.issue_id.clone(), acc, "why", "agent");
    IssueLog::open(store.root()).unwrap().put_proposal(&p).unwrap();
    p
}

#[test]
fn pending_proposal_does_not_change_the_contract() {
    let (store, _t) = fresh();
    let issue = free_form(&store);
    let p = propose(&store, &issue, clamp_delta());
    assert_eq!(proposal_status(&store, &p.proposal_id).unwrap(), ProposalStatus::Pending);
    assert_eq!(effective_acceptance(&store, &issue).unwrap(), (Acceptance::FreeForm {}, None));
}

#[test]
fn approval_sets_effective_acceptance_under_the_same_id() {
    let (store, _t) = fresh();
    let head = publish(&store, "fn one() -> Int { 1 }\n");
    let issue = free_form(&store);
    let p = propose(&store, &issue, clamp_delta());
    record_proposal_review(&store, &p, "alfonso", true, None).unwrap();
    assert_eq!(proposal_status(&store, &p.proposal_id).unwrap(), ProposalStatus::Approved);
    let (acc, via) = effective_acceptance(&store, &issue).unwrap();
    assert_eq!(acc, clamp_delta());
    assert_eq!(via.as_deref(), Some(p.proposal_id.as_str()));

    let judged = with_effective_acceptance(&store, &issue).unwrap();
    assert_eq!(judged.issue_id, issue.issue_id, "the verdict still keys on the issue");
    // Now machine-evaluable: clamp is absent at head, so the gate fails it
    // rather than calling the issue inconclusive.
    assert!(matches!(evaluate_static(&store, &judged, &head).unwrap(), IssueEvaluation::Failed { .. }));
    assert!(matches!(
        evaluate_static(&store, &issue, &head).unwrap(),
        IssueEvaluation::NotEvaluable { .. }
    ));
}

#[test]
fn rejection_keeps_the_issue_free_form_and_the_history() {
    let (store, _t) = fresh();
    let issue = free_form(&store);
    let p = propose(&store, &issue, clamp_delta());
    record_proposal_review(&store, &p, "alfonso", false, Some("too narrow".into())).unwrap();
    assert_eq!(proposal_status(&store, &p.proposal_id).unwrap(), ProposalStatus::Rejected);
    assert_eq!(effective_acceptance(&store, &issue).unwrap().0, Acceptance::FreeForm {});
    let history = IssueLog::open(store.root()).unwrap().proposals_for(&issue.issue_id).unwrap();
    assert_eq!(history, vec![p], "a rejected proposal stays in the log");
}

#[test]
fn same_second_approve_and_reject_resolves_to_rejected() {
    let (store, _t) = fresh();
    let issue = free_form(&store);
    let p = propose(&store, &issue, clamp_delta());
    record_proposal_review(&store, &p, "a", true, None).unwrap();
    record_proposal_review(&store, &p, "b", false, None).unwrap();
    // Both reviews almost surely share a second; if they don't, the later
    // one (the rejection) wins anyway. Either way: rejected.
    assert_eq!(proposal_status(&store, &p.proposal_id).unwrap(), ProposalStatus::Rejected);
}

#[test]
fn a_later_review_supersedes_an_earlier_one() {
    let (store, _t) = fresh();
    let issue = free_form(&store);
    let p = propose(&store, &issue, clamp_delta());
    record_proposal_review(&store, &p, "alfonso", false, None).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    record_proposal_review(&store, &p, "alfonso", true, None).unwrap();
    assert_eq!(proposal_status(&store, &p.proposal_id).unwrap(), ProposalStatus::Approved);
    assert_eq!(effective_acceptance(&store, &issue).unwrap().0, clamp_delta());
}

#[test]
fn typed_issues_are_not_refinable() {
    let typed = Issue::with_timestamp("t", "", clamp_delta(), None, BTreeSet::new(), None, 1);
    assert!(matches!(check_refinable(&typed), Err(StoreError::IssueRefinement(_))));
    let ff = Issue::with_timestamp("t", "", Acceptance::FreeForm {}, None, BTreeSet::new(), None, 1);
    assert!(check_refinable(&ff).is_ok());
}
