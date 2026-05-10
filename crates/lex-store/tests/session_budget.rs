//! Conformance tests for #292 slice 1 — per-session budget ledger.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
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

/// Persist an Intent and return its IntentId.
fn create_intent(store: &Store, prompt: &str, session: &str) -> String {
    let intent = lex_vcs::Intent::new(prompt, session, anthropic(), None);
    let id = intent.intent_id.clone();
    lex_vcs::IntentLog::open(store.root()).unwrap()
        .put(&intent).unwrap();
    id
}

/// Add a fn with the given budget cost, tagged with `intent_id`.
/// Chains off the current branch head so multiple calls compose.
fn add_fn_with_budget(
    store: &Store,
    sig: &str,
    stage: &str,
    cost: u64,
    intent_id: Option<&str>,
) {
    let mut effects = BTreeSet::new();
    effects.insert(format!("budget({cost})"));
    let parents: Vec<lex_vcs::OpId> = store
        .get_branch(DEFAULT_BRANCH).unwrap()
        .and_then(|b| b.head_op)
        .into_iter().collect();
    let mut op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stage.into(),
            effects,
            budget_cost: Some(cost),
        },
        parents,
    );
    if let Some(id) = intent_id {
        op = op.with_intent(id);
    }
    let t = StageTransition::Create {
        sig_id: sig.into(),
        stage_id: stage.into(),
    };
    store.apply_operation(DEFAULT_BRANCH, op, t).unwrap();
}

#[test]
fn fresh_store_reports_zero_for_unknown_session() {
    let (store, _tmp) = fresh();
    let b = store.session_budget("unknown-session").unwrap();
    assert_eq!(b.session_id, "unknown-session");
    assert_eq!(b.spent, 0);
    assert_eq!(b.op_count, 0);
}

#[test]
fn fresh_store_reports_no_session_rollups() {
    let (store, _tmp) = fresh();
    let all = store.all_session_budgets().unwrap();
    assert!(all.is_empty());
}

#[test]
fn add_function_with_budget_attributed_to_session() {
    let (store, _tmp) = fresh();
    let intent = create_intent(&store, "build a calculator", "ses_alpha");
    add_fn_with_budget(&store, "fac", "stg-1", 10, Some(&intent));

    let b = store.session_budget("ses_alpha").unwrap();
    assert_eq!(b.spent, 10);
    assert_eq!(b.op_count, 1);
}

#[test]
fn ops_without_intent_are_excluded() {
    let (store, _tmp) = fresh();
    // No intent_id on the op → no session to attribute to.
    add_fn_with_budget(&store, "fac", "stg-1", 10, None);

    let all = store.all_session_budgets().unwrap();
    assert!(all.is_empty(),
        "ops without intent_id must not contribute; got {all:?}");
}

#[test]
fn multiple_sessions_kept_separate() {
    let (store, _tmp) = fresh();
    let alpha = create_intent(&store, "build calculator", "ses_alpha");
    let beta = create_intent(&store, "fix bug", "ses_beta");

    add_fn_with_budget(&store, "fac", "stg-1", 5, Some(&alpha));
    add_fn_with_budget(&store, "fib", "stg-2", 7, Some(&beta));
    add_fn_with_budget(&store, "sum", "stg-3", 3, Some(&alpha));

    let alpha_b = store.session_budget("ses_alpha").unwrap();
    assert_eq!(alpha_b.spent, 5 + 3);
    assert_eq!(alpha_b.op_count, 2);

    let beta_b = store.session_budget("ses_beta").unwrap();
    assert_eq!(beta_b.spent, 7);
    assert_eq!(beta_b.op_count, 1);

    let all = store.all_session_budgets().unwrap();
    assert_eq!(all.len(), 2);
    let sessions: Vec<&str> = all.iter().map(|b| b.session_id.as_str()).collect();
    assert_eq!(sessions, vec!["ses_alpha", "ses_beta"],
        "sorted by session_id");
}

#[test]
fn modify_body_increase_contributes_delta_only() {
    let (store, _tmp) = fresh();
    let alpha = create_intent(&store, "tune", "ses_alpha");
    // First op: AddFunction with budget=5 → spend = 5.
    add_fn_with_budget(&store, "fac", "stg-1", 5, Some(&alpha));

    // Second op: ModifyBody from_budget=5 to_budget=12 → spend
    // contribution = 12 - 5 = 7.
    let parent = store.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
    let mod_op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
            from_budget: Some(5),
            to_budget: Some(12),
        },
        [parent],
    ).with_intent(&alpha);
    let t = StageTransition::Replace {
        sig_id: "fac".into(),
        from: "stg-1".into(),
        to: "stg-2".into(),
    };
    store.apply_operation(DEFAULT_BRANCH, mod_op, t).unwrap();

    let b = store.session_budget("ses_alpha").unwrap();
    assert_eq!(b.spent, 5 + 7);
    assert_eq!(b.op_count, 2);
}

#[test]
fn modify_body_decrease_does_not_refund() {
    let (store, _tmp) = fresh();
    let alpha = create_intent(&store, "tune", "ses_alpha");
    add_fn_with_budget(&store, "fac", "stg-1", 20, Some(&alpha));

    let parent = store.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
    let mod_op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
            from_budget: Some(20),
            to_budget: Some(5),
        },
        [parent],
    ).with_intent(&alpha);
    let t = StageTransition::Replace {
        sig_id: "fac".into(),
        from: "stg-1".into(),
        to: "stg-2".into(),
    };
    store.apply_operation(DEFAULT_BRANCH, mod_op, t).unwrap();

    let b = store.session_budget("ses_alpha").unwrap();
    // Only the initial 20 counts; the decrease doesn't deduct.
    assert_eq!(b.spent, 20);
    // op_count is just the ops that contributed a non-zero
    // increment; the decrease contributed 0 so it's not counted.
    assert_eq!(b.op_count, 1);
}

#[test]
fn intent_not_in_log_is_skipped_gracefully() {
    let (store, _tmp) = fresh();
    // Tag the op with an intent_id that doesn't exist in the
    // IntentLog. Real workflows always persist the intent first,
    // but the ledger should degrade gracefully.
    add_fn_with_budget(&store, "fac", "stg-1", 10, Some("dangling-intent"));

    let all = store.all_session_budgets().unwrap();
    assert!(all.is_empty(),
        "dangling intent → no session attribution; got {all:?}");
}
