//! Performance budget for #133: branch create + discard is `O(1)`
//! per branch regardless of op-log size.
//!
//! The issue's full target is "100 branches in a 10k-op store
//! < 1s." We use a 1k-op fixture here because building 10k ops
//! through the public `apply_operation` pipeline takes ~40s on
//! GHA, which would dominate CI time. 1k ops is plenty to catch
//! a *quadratic* regression in branch ops (the architectural
//! risk we care about); a bench tool with criterion would be
//! the right home for full-scale numbers and is left as a
//! follow-up.
//!
//! Budget: 100 create+delete cycles against a 1k-op store
//! complete in < 1 second.

use std::collections::BTreeSet;
use std::time::Instant;
use tempfile::tempdir;

use lex_store::{Store, DEFAULT_BRANCH};
use lex_vcs::{Operation, OperationKind, StageTransition};

const OP_COUNT: usize = 1_000;
const BRANCH_COUNT: usize = 100;

/// Build a Store with `OP_COUNT` ops via `Store::apply_operation`
/// (the public path that exercises the same write pipeline a real
/// agent would). Linear DAG: each op's parent is the previous head.
fn build_op_store() -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    for i in 0..OP_COUNT {
        let sig = format!("fn::sig_{i}");
        let stage = format!("stage_{i}");
        let head_now = store.get_branch(DEFAULT_BRANCH).unwrap()
            .and_then(|b| b.head_op);
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.clone(),
                stage_id: stage.clone(),
                effects: BTreeSet::new(),
                budget_cost: None,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        );
        let transition = StageTransition::Create { sig_id: sig, stage_id: stage };
        store.apply_operation(DEFAULT_BRANCH, op, transition).unwrap();
    }
    tmp
}

#[test]
fn create_and_discard_100_branches_is_fast() {
    let tmp = build_op_store();
    let store = Store::open(tmp.path()).unwrap();

    let start = Instant::now();
    for i in 0..BRANCH_COUNT {
        let name = format!("perf_{i}");
        store.create_branch(&name, DEFAULT_BRANCH).unwrap();
        store.delete_branch(&name).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 1.0,
        "{BRANCH_COUNT} create+delete cycles took {elapsed:?}; budget is < 1s (issue #133)"
    );
}
