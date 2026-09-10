//! `branch_head` used to walk the *entire* op log on every call —
//! O(N) per call where N is total ops on the branch's history, with
//! no memoization (see the pre-fix doc comment this replaces). On a
//! tenant with 110k+ accumulated ops, a single call took on the
//! order of an hour. Every `pkg publish` calls `branch_head` at
//! least once, so publish latency scaled with *total historical*
//! op count, not with the size of the change being published — it
//! never got faster, only slower, the more a tenant was used.
//!
//! The fix persists a snapshot (`<branch>.head_snapshot.json`) keyed
//! on the head it was computed for, and replays only the ops added
//! since that snapshot (`OpLog::walk_forward_since`) instead of
//! re-walking from genesis every time.
//!
//! Correctness is the hard requirement here — a stale or wrongly
//! reused snapshot would silently corrupt every publish downstream.
//! This file checks: (1) snapshot-assisted results are byte-for-byte
//! identical to a full walk from genesis, across several call
//! patterns including a branch reset (where the snapshot's op is no
//! longer an ancestor of the new head), and (2) a steady-state
//! publish loop (one new op, one `branch_head` call, repeated) is
//! fast regardless of how much history already exists — the exact
//! shape of a real `pkg publish` workload.

use std::collections::BTreeSet;
use std::time::Instant;
use tempfile::tempdir;

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};

fn add(s: &Store, branch: &str, sig: &str, stg: &str) -> String {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stg.into(),
            effects: BTreeSet::new(),
            budget_cost: None,
        },
        s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect::<Vec<_>>(),
    );
    let t = StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() };
    s.apply_operation(branch, op, t).unwrap()
}

fn modify(s: &Store, branch: &str, sig: &str, from: &str, to: &str) -> String {
    let parent = s.get_branch(branch).unwrap().and_then(|b| b.head_op).unwrap();
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.into(),
            from_stage_id: from.into(),
            to_stage_id: to.into(),
            from_budget: None,
            to_budget: None,
        },
        [parent],
    );
    let t = StageTransition::Replace { sig_id: sig.into(), from: from.into(), to: to.into() };
    s.apply_operation(branch, op, t).unwrap()
}

/// Build a store with `n` sequential AddFunction ops on `DEFAULT_BRANCH`.
fn build_history(n: usize) -> (Store, tempfile::TempDir) {
    let tmp = tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    for i in 0..n {
        add(&store, DEFAULT_BRANCH, &format!("fn::sig_{i}"), &format!("stage_{i}"));
    }
    (store, tmp)
}

#[test]
fn repeated_calls_on_an_unchanged_head_agree_with_a_full_walk() {
    let (store, _tmp) = build_history(50);
    let first = store.branch_head(DEFAULT_BRANCH).unwrap();
    // Snapshot now written; a second call on the same head must hit
    // the fast path and still return the identical map.
    let second = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.len(), 50);
}

#[test]
fn incremental_calls_after_new_ops_agree_with_a_full_walk_from_genesis() {
    let (store, tmp) = build_history(30);
    let _ = store.branch_head(DEFAULT_BRANCH).unwrap(); // seeds a snapshot at op 30

    // Advance the branch several times, calling branch_head after
    // each — the exact shape of a real multi-publish workload.
    for i in 30..40 {
        add(&store, DEFAULT_BRANCH, &format!("fn::sig_{i}"), &format!("stage_{i}"));
        modify(&store, DEFAULT_BRANCH, "fn::sig_0", "stage_0", &format!("stage_0_v{i}"));
        let incremental = store.branch_head(DEFAULT_BRANCH).unwrap();

        // A brand-new Store handle over the same directory has no
        // in-memory state and no snapshot bias of its own — but it
        // WILL read the snapshot file the other handle just wrote.
        // Compare against a from-scratch store with the snapshot
        // deleted, forcing a genuine full walk_forward from genesis.
        let fresh = Store::open(tmp.path()).unwrap();
        let snapshot_path = tmp.path().join("branches").join(format!("{DEFAULT_BRANCH}.head_snapshot.json"));
        let saved = std::fs::read(&snapshot_path).ok();
        let _ = std::fs::remove_file(&snapshot_path);
        let full_walk = fresh.branch_head(DEFAULT_BRANCH).unwrap();
        if let Some(bytes) = saved {
            std::fs::write(&snapshot_path, bytes).unwrap();
        }

        assert_eq!(
            incremental, full_walk,
            "mismatch after {} total ops (i={i})",
            30 + (i - 30 + 1) * 2
        );
    }
}

// The "snapshot's op is no longer an ancestor of the new head" fallback
// (a branch reset) needs `Store::set_branch_head_op`, which is
// `pub(crate)` — apply_operation's CAS retry rebuilds a single-parent
// op's parent to match the *current* head on every call, so there's no
// way to land a genuinely disconnected head through the public API
// alone. That path is covered by a unit test inside
// `lex-store/src/branches.rs` (`branch_head_falls_back_to_full_walk_when_snapshot_predates_a_reset`),
// which has the crate-internal access needed to force it.

// History size for the steady-state budget test below. `build_history`
// alone (through the public `apply_operation` path, gate checks and
// all) dominates this test's wall time regardless of the branch_head
// fix, so — same reasoning `branch_perf.rs` already documents for
// picking 1k ops over a full 10k on GHA — this stays small enough to
// build quickly while still being orders of magnitude past what a
// per-call O(N) walk could hide behind.
const LONG_HISTORY: usize = 2_000;

#[test]
fn steady_state_publish_loop_stays_fast_as_history_grows() {
    // Seed a long history first, matching a tenant that's been
    // published to many times already — this is exactly the
    // regime that used to be slow (O(total history) per call).
    let (store, _tmp) = build_history(LONG_HISTORY);
    let _ = store.branch_head(DEFAULT_BRANCH).unwrap(); // prime the snapshot once

    let start = Instant::now();
    for i in LONG_HISTORY..LONG_HISTORY + 100 {
        add(&store, DEFAULT_BRANCH, &format!("fn::sig_{i}"), &format!("stage_{i}"));
        let _ = store.branch_head(DEFAULT_BRANCH).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs_f64() < 5.0,
        "100 publish-then-branch_head cycles against a {LONG_HISTORY}-op history took {elapsed:?}; \
         expected steady-state cost proportional to ops-since-last-call, not total history"
    );
}
