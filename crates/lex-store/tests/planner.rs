//! Conformance for `Store::plan` — the cost-aware planner (#307).

use lex_ast::{canonicalize_program, sig_id, stage_id, Stage};
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use tempfile::TempDir;

fn fresh() -> (Store, TempDir) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn publish_fn(store: &Store, src: &str, name: &str) {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let stage = stages
        .into_iter()
        .find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == name))
        .expect("stage not found");
    let sig = sig_id(&stage).unwrap();
    let stg = stage_id(&stage).unwrap();
    // The planner reads its budget data from the stage's AST, not
    // the op record, so the precise op-effect set doesn't matter.
    store.publish(&stage).unwrap();
    let op = lex_vcs::Operation::new(
        lex_vcs::OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stg.clone(),
            effects: Default::default(),
            budget_cost: None,
        },
        [],
    );
    let t = lex_vcs::StageTransition::Create {
        sig_id: sig,
        stage_id: stg,
    };
    store.apply_operation(DEFAULT_BRANCH, op, t).unwrap();
}

const CHAIN_SRC: &str = r#"
fn leaf() -> [budget(100)] Int { 7 }
fn mid() -> [budget(200)] Int { leaf() }
fn root() -> [budget(50)] Int { mid() }
"#;

#[test]
fn three_fn_chain_sums_budget() {
    // AC: a 3-fn chain with declared budgets produces the expected total.
    let (store, _tmp) = fresh();
    publish_fn(&store, CHAIN_SRC, "leaf");
    publish_fn(&store, CHAIN_SRC, "mid");
    publish_fn(&store, CHAIN_SRC, "root");

    let plan = store.plan(DEFAULT_BRANCH, "root", None, None).unwrap();
    assert_eq!(plan.goal, "root");
    assert_eq!(plan.paths.len(), 1, "single linear chain: {:?}", plan.paths);
    let path = &plan.paths[0];
    assert_eq!(path.chain, vec!["root", "mid", "leaf"]);
    assert_eq!(path.total_cost, 50 + 200 + 100);
    assert!(path.fits, "no cap -> every path fits");
}

#[test]
fn max_cost_marks_overflow_paths_as_would_exceed() {
    let (store, _tmp) = fresh();
    publish_fn(&store, CHAIN_SRC, "leaf");
    publish_fn(&store, CHAIN_SRC, "mid");
    publish_fn(&store, CHAIN_SRC, "root");

    // Total cost is 350; cap of 200 doesn't fit.
    let plan = store
        .plan(DEFAULT_BRANCH, "root", Some(200), None)
        .unwrap();
    assert_eq!(plan.effective_cap, Some(200));
    assert!(!plan.paths[0].fits);

    // Cap of 1000 does fit.
    let plan = store
        .plan(DEFAULT_BRANCH, "root", Some(1000), None)
        .unwrap();
    assert!(plan.paths[0].fits);
}

const BRANCHING_SRC: &str = r#"
fn cheap_leaf() -> [budget(10)] Int { 1 }
fn expensive_leaf() -> [budget(900)] Int { 2 }
fn branch_pick(n :: Int) -> [budget(5)] Int { match n { 0 => cheap_leaf(), _ => expensive_leaf() } }
"#;

#[test]
fn branching_fn_emits_multiple_paths_sorted_cheapest_first() {
    let (store, _tmp) = fresh();
    publish_fn(&store, BRANCHING_SRC, "cheap_leaf");
    publish_fn(&store, BRANCHING_SRC, "expensive_leaf");
    publish_fn(&store, BRANCHING_SRC, "branch_pick");

    let plan = store
        .plan(DEFAULT_BRANCH, "branch_pick", Some(100), None)
        .unwrap();
    assert_eq!(plan.paths.len(), 2, "two match arms -> two paths");
    // Cheapest first.
    assert_eq!(plan.paths[0].chain, vec!["branch_pick", "cheap_leaf"]);
    assert_eq!(plan.paths[0].total_cost, 5 + 10);
    assert!(plan.paths[0].fits, "5+10=15 fits in cap=100");

    assert_eq!(plan.paths[1].chain, vec!["branch_pick", "expensive_leaf"]);
    assert_eq!(plan.paths[1].total_cost, 5 + 900);
    assert!(!plan.paths[1].fits, "5+900=905 exceeds cap=100");
}

const RECURSIVE_SRC: &str = r#"
fn recur(n :: Int) -> [budget(7)] Int { match n { 0 => 0, _ => recur(n) } }
"#;

#[test]
fn recursive_self_call_is_budgeted_once() {
    // AC: recursive functions are detected and budgeted with their
    // declared self-cost (no infinite-loop expansion).
    let (store, _tmp) = fresh();
    publish_fn(&store, RECURSIVE_SRC, "recur");

    let plan = store
        .plan(DEFAULT_BRANCH, "recur", Some(1000), None)
        .unwrap();
    // The chain terminates at the second visit; cost is one
    // self-budget (7), not infinite.
    assert!(
        plan.paths.iter().all(|p| p.total_cost == 7),
        "recursive cost must be capped at one self-visit: {:?}",
        plan.paths
    );
    assert!(plan.paths.iter().all(|p| p.chain == vec!["recur"]));
}

const EFFECT_SRC: &str = r#"
import "std.io" as io
fn echo(s :: Str) -> [io, budget(3)] Nil { io.print(s) }
fn caller(s :: Str) -> [io, budget(1)] Nil { echo(s) }
"#;

#[test]
fn path_effects_are_union_of_chain() {
    // AC: effect-set on each path is reported so the agent can also
    // gate by policy.
    let (store, _tmp) = fresh();
    publish_fn(&store, EFFECT_SRC, "echo");
    publish_fn(&store, EFFECT_SRC, "caller");

    let plan = store
        .plan(DEFAULT_BRANCH, "caller", None, None)
        .unwrap();
    assert_eq!(plan.paths.len(), 1);
    let p = &plan.paths[0];
    assert_eq!(p.chain, vec!["caller", "echo"]);
    // `io` flows through both. `budget` is the cost dimension, so
    // it's intentionally excluded from the effects set.
    assert!(p.effects.contains("io"));
    assert!(!p.effects.contains("budget"));
}

#[test]
fn unknown_goal_yields_empty_paths() {
    let (store, _tmp) = fresh();
    publish_fn(&store, CHAIN_SRC, "leaf");
    let plan = store.plan(DEFAULT_BRANCH, "nonexistent", None, None).unwrap();
    assert!(plan.paths.is_empty());
}
