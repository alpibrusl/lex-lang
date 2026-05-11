//! Conformance tests for #292 slices 2 + 3 — `policy.session_budgets`
//! schema + apply-path gate.

use lex_store::{
    policy::{PolicyFile, SessionBudgetPolicy},
    Operation, OperationKind, StageTransition, Store, StoreError, DEFAULT_BRANCH,
};
use std::collections::BTreeSet;
use tempfile::TempDir;

fn fresh() -> (Store, TempDir) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn anthropic() -> lex_vcs::ModelDescriptor {
    lex_vcs::ModelDescriptor {
        provider: "anthropic".into(),
        name: "claude-test".into(),
        version: None,
    }
}

fn create_intent(store: &Store, prompt: &str, session: &str) -> String {
    let intent = lex_vcs::Intent::new(prompt, session, anthropic(), None);
    let id = intent.intent_id.clone();
    lex_vcs::IntentLog::open(store.root()).unwrap().put(&intent).unwrap();
    id
}

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    let prog = lex_syntax::parse_source(src).expect("parse");
    lex_ast::canonicalize_program(&prog)
}

fn well_typed_add_op(sig: &str, stage: &str, cost: u64, intent: &str) -> (Operation, StageTransition) {
    let mut effects = BTreeSet::new();
    effects.insert(format!("budget({cost})"));
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stage.into(),
            effects,
            budget_cost: Some(cost),
        },
        [],
    ).with_intent(intent);
    let t = StageTransition::Create {
        sig_id: sig.into(),
        stage_id: stage.into(),
    };
    (op, t)
}

#[test]
fn budget_lookup_falls_back_to_default_cap() {
    let (store, _tmp) = fresh();
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            default_cap: Some(100),
            ..Default::default()
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();
    assert_eq!(store.session_budget_cap("any-session").unwrap(), Some(100));
}

#[test]
fn budget_lookup_per_session_override_wins() {
    let (store, _tmp) = fresh();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("ses_alpha".into(), Some(200));
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            default_cap: Some(100),
            overrides,
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();
    assert_eq!(store.session_budget_cap("ses_alpha").unwrap(), Some(200));
    assert_eq!(store.session_budget_cap("other").unwrap(), Some(100));
}

#[test]
fn budget_lookup_explicit_null_override_means_unbounded() {
    let (store, _tmp) = fresh();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("ses_human".into(), None);
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            default_cap: Some(100),
            overrides,
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();
    assert_eq!(store.session_budget_cap("ses_human").unwrap(), None,
        "explicit null override = unbounded");
}

#[test]
fn session_budget_envelope_includes_cap_and_remaining() {
    let (store, _tmp) = fresh();
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            default_cap: Some(50),
            ..Default::default()
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();

    let b = store.session_budget("ses_new").unwrap();
    assert_eq!(b.spent, 0);
    assert_eq!(b.cap, Some(50));
    assert_eq!(b.remaining, Some(50));
}

#[test]
fn apply_operation_checked_rejects_op_that_would_exceed_cap() {
    let (store, _tmp) = fresh();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("ses_tight".into(), Some(10));
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            overrides,
            ..Default::default()
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();
    let intent_id = create_intent(&store, "expensive work", "ses_tight");

    // Op claims budget=20, cap=10 → refuse.
    let candidate = parse("fn pricey() -> Int { 1 }\n");
    let (op, t) = well_typed_add_op("pricey", "stg-1", 20, &intent_id);
    let err = store.apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .unwrap_err();
    match err {
        StoreError::BudgetExceeded { session_id, cap, spent_after } => {
            assert_eq!(session_id, "ses_tight");
            assert_eq!(cap, 10);
            assert_eq!(spent_after, 20);
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }

    // Branch head must be unchanged — gate fires before the apply.
    assert!(store.get_branch(DEFAULT_BRANCH).unwrap().is_none(),
        "rejected op must not create the branch");
}

#[test]
fn apply_operation_checked_allows_op_within_cap() {
    let (store, _tmp) = fresh();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("ses_alpha".into(), Some(100));
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy { overrides, ..Default::default() },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();
    let intent = create_intent(&store, "tractable", "ses_alpha");

    let candidate = parse("fn cheap() -> Int { 1 }\n");
    let (op, t) = well_typed_add_op("cheap", "stg-1", 10, &intent);
    let op_id = store.apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect("op within cap should land");

    let b = store.session_budget("ses_alpha").unwrap();
    assert_eq!(b.spent, 10);
    assert_eq!(b.remaining, Some(90));
    // Branch head advanced.
    let head = store.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op;
    assert_eq!(head.as_deref(), Some(op_id.as_str()));
}

#[test]
fn apply_operation_checked_no_intent_is_unaffected() {
    let (store, _tmp) = fresh();
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            default_cap: Some(5),
            ..Default::default()
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();

    // No intent_id on the op → no session to gate. Even with a
    // tiny default_cap, the op lands.
    let candidate = parse("fn untagged() -> Int { 1 }\n");
    let mut effects = BTreeSet::new();
    effects.insert("budget(50)".into());
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: "untagged".into(),
            stage_id: "stg-1".into(),
            effects,
            budget_cost: Some(50),
        },
        [],
    );
    let t = StageTransition::Create {
        sig_id: "untagged".into(),
        stage_id: "stg-1".into(),
    };
    store.apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect("intent-less ops sail through the budget gate");
}

#[test]
fn apply_operation_checked_unbounded_override_disables_default_cap() {
    let (store, _tmp) = fresh();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("ses_human".into(), None);
    let policy = PolicyFile {
        session_budgets: SessionBudgetPolicy {
            default_cap: Some(5),
            overrides,
        },
        ..Default::default()
    };
    lex_store::policy::save(store.root(), &policy).unwrap();
    let intent = create_intent(&store, "manual override", "ses_human");

    let candidate = parse("fn big() -> Int { 1 }\n");
    let (op, t) = well_typed_add_op("big", "stg-1", 1000, &intent);
    store.apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect("unbounded override sails past the default cap");
}

#[test]
fn no_policy_means_no_enforcement() {
    let (store, _tmp) = fresh();
    let intent = create_intent(&store, "anything", "ses_alpha");
    // No policy.json at all. Even a huge op lands.
    let candidate = parse("fn anything() -> Int { 1 }\n");
    let (op, t) = well_typed_add_op("anything", "stg-1", 9999, &intent);
    store.apply_operation_checked(DEFAULT_BRANCH, op, t, &candidate)
        .expect("no policy = no enforcement");
}
