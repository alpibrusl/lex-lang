//! Conformance tests for #262 multi-writer CAS on branch advance.
//!
//! Pre-#262, two writers calling `apply_operation` against the same
//! branch concurrently could either lose one another's update (last
//! write wins on the branch file) or land in an inconsistent state.
//! With #262, branch advance is a CAS guarded by `fs2` advisory
//! locking on a per-branch lockfile, and `apply_operation` retries
//! up to 8 times on contention before surfacing
//! `StoreError::Contention`.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::thread;

fn fresh() -> (Arc<Store>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (Arc::new(s), tmp)
}

#[test]
fn n_concurrent_writers_all_land() {
    // Spawn N threads, each calling `apply_operation` with a unique
    // signature. After all threads finish, the head_state must
    // contain every signature — none lost to races.
    const N: usize = 12;
    let (s, _tmp) = fresh();

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let s = Arc::clone(&s);
            thread::spawn(move || {
                let sig = format!("sig-{}", i);
                let stage = format!("stg-{}", i);
                let op = Operation::new(
                    OperationKind::AddFunction {
                        sig_id: sig.clone(),
                        stage_id: stage.clone(),
                        effects: BTreeSet::new(),
                        budget_cost: None,
                    },
                    [],
                );
                let t = StageTransition::Create {
                    sig_id: sig.clone(),
                    stage_id: stage.clone(),
                };
                s.apply_operation(DEFAULT_BRANCH, op, t)
                    .expect("apply should succeed under contention")
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.len(), N, "every writer's signature must be present");
    for i in 0..N {
        assert_eq!(head.get(&format!("sig-{}", i)), Some(&format!("stg-{}", i)));
    }
}

#[test]
fn n_concurrent_writers_chain_into_a_single_history() {
    // After N concurrent writers, the op-log must form a single
    // chain of length N rooted at the genesis op (parents=[]).
    // Each non-root op has exactly one parent, and the chain
    // terminates at the branch head.
    const N: usize = 8;
    let (s, _tmp) = fresh();

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let s = Arc::clone(&s);
            thread::spawn(move || {
                let sig = format!("sig-{}", i);
                let stage = format!("stg-{}", i);
                let op = Operation::new(
                    OperationKind::AddFunction {
                        sig_id: sig.clone(),
                        stage_id: stage.clone(),
                        effects: BTreeSet::new(),
                        budget_cost: None,
                    },
                    [],
                );
                let t = StageTransition::Create {
                    sig_id: sig.clone(),
                    stage_id: stage.clone(),
                };
                s.apply_operation(DEFAULT_BRANCH, op, t).unwrap()
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Walk back from head and count nodes; verify single linear chain.
    let log = lex_vcs::OpLog::open(s.root()).unwrap();
    let head_op = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap().head_op.unwrap();
    let mut cursor = Some(head_op);
    let mut count = 0;
    while let Some(id) = cursor {
        let rec = log.get(&id).unwrap().unwrap();
        count += 1;
        cursor = match rec.op.parents.len() {
            0 => None,
            1 => Some(rec.op.parents[0].clone()),
            n => panic!("unexpected fan-in of {} at op {}", n, id),
        };
    }
    assert_eq!(count, N, "history should be a linear chain of N ops");
}

#[test]
fn concurrent_writers_do_not_lose_op_records() {
    // Even on contention paths where the final op_id changes after
    // a CAS-mismatch retry (because the rebuilt op has a new
    // parent), the *intermediate* persisted op record is still on
    // disk. This test ensures we don't silently drop op records on
    // retry — we just don't reference them from the branch head.
    const N: usize = 6;
    let (s, tmp) = fresh();

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let s = Arc::clone(&s);
            thread::spawn(move || {
                let sig = format!("sig-{}", i);
                let stage = format!("stg-{}", i);
                let op = Operation::new(
                    OperationKind::AddFunction {
                        sig_id: sig.clone(),
                        stage_id: stage.clone(),
                        effects: BTreeSet::new(),
                        budget_cost: None,
                    },
                    [],
                );
                let t = StageTransition::Create {
                    sig_id: sig.clone(),
                    stage_id: stage.clone(),
                };
                s.apply_operation(DEFAULT_BRANCH, op, t).unwrap()
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // The ops/ directory should contain at least N op records (one
    // per logical write). It may contain *more* if any writer was
    // forced to retry — those orphan records are intentional under
    // the append-only contract.
    let ops_dir = tmp.path().join("ops");
    let n_ops = std::fs::read_dir(&ops_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert!(
        n_ops >= N,
        "ops/ should hold at least {} records (got {})",
        N,
        n_ops
    );
}
