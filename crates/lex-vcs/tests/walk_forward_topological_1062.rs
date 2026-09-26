//! #1062: `OpLog::walk_forward` is the order every head is replayed in, so it
//! has to be a topological order of the ancestry. It used to be the reverse of
//! a breadth-first walk, which is not: when a merge joins lines of different
//! lengths, an op reached by a short path is emitted before its own
//! descendant on the long one, so replay applied an old change over a newer
//! one.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use lex_vcs::{OpId, OpLog, Operation, OperationKind, OperationRecord, StageTransition};

fn put(log: &OpLog, kind: OperationKind, parents: &[&OpId], t: StageTransition) -> OpId {
    let rec = OperationRecord::new(Operation::new(kind, parents.iter().map(|p| (*p).clone()).collect::<Vec<_>>()), t);
    log.put(&rec).unwrap();
    rec.op_id
}

fn create(log: &OpLog, parents: &[&OpId], sig: &str, stg: &str) -> OpId {
    put(
        log,
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stg.into(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        },
        parents,
        StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() },
    )
}

fn replace(log: &OpLog, parent: &OpId, sig: &str, from: &str, to: &str) -> OpId {
    put(
        log,
        OperationKind::ModifyBody {
            sig_id: sig.into(),
            from_stage_id: from.into(),
            to_stage_id: to.into(),
            from_budget: None,
            to_budget: None,
            to_sig_id: None,
        },
        &[parent],
        StageTransition::Replace { sig_id: sig.into(), from: from.into(), to: to.into() },
    )
}

fn merge(log: &OpLog, a: &OpId, b: &OpId) -> OpId {
    put(
        log,
        OperationKind::Merge { resolved: 0 },
        &[a, b],
        StageTransition::Merge { entries: BTreeMap::new() },
    )
}

/// The pre-fix `walk_forward`: BFS from the head, reversed.
fn legacy_forward(log: &OpLog, head: &OpId) -> Vec<OperationRecord> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut frontier: VecDeque<OpId> = VecDeque::from([head.clone()]);
    while let Some(id) = frontier.pop_back() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(rec) = log.get(&id).unwrap() {
            for p in &rec.op.parents {
                if !seen.contains(p) {
                    frontier.push_front(p.clone());
                }
            }
            out.push(rec);
        }
    }
    out.reverse();
    out
}

fn is_topological(order: &[OperationRecord]) -> bool {
    let pos: BTreeMap<&str, usize> = order.iter().enumerate().map(|(i, r)| (r.op_id.as_str(), i)).collect();
    order
        .iter()
        .enumerate()
        .all(|(i, r)| r.op.parents.iter().all(|p| pos.get(p.as_str()).is_none_or(|&j| j < i)))
}

fn ids(order: &[OperationRecord]) -> Vec<&str> {
    order.iter().map(|r| r.op_id.as_str()).collect()
}

/// The specification of the repaired order, written the slow obvious way:
/// repeatedly take the earliest op (in the old BFS order) whose parents have
/// all been taken. This pins the tie-break — a different-but-valid
/// topological order would replay unpinned merges differently.
fn reference_linearization(old: &[OperationRecord]) -> Vec<String> {
    let present: BTreeSet<&str> = old.iter().map(|r| r.op_id.as_str()).collect();
    let mut taken: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    while out.len() < old.len() {
        let next = old
            .iter()
            .find(|r| {
                !taken.contains(r.op_id.as_str())
                    && r.op.parents.iter().all(|p| !present.contains(p.as_str()) || taken.contains(p.as_str()))
            })
            .expect("a DAG always has a ready op");
        taken.insert(next.op_id.as_str());
        out.push(next.op_id.clone());
    }
    out
}

fn replay(order: &[OperationRecord]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for r in order {
        match &r.produces {
            StageTransition::Create { sig_id, stage_id } | StageTransition::Replace { sig_id, to: stage_id, .. } => {
                m.insert(sig_id.clone(), stage_id.clone());
            }
            StageTransition::Remove { sig_id, .. } => {
                m.remove(sig_id);
            }
            _ => {}
        }
    }
    m
}

#[test]
fn uneven_diamond_replays_an_ancestor_before_its_descendants() {
    // P creates x; a long line modifies it three ops later; a short line
    // hangs one op off P. The merge joins them.
    //
    //     P --- l1 --- l2 --- l3 ---+
    //      \                        M
    //       `-- s ------------------+
    let tmp = tempfile::tempdir().unwrap();
    let log = OpLog::open(tmp.path()).unwrap();
    let p = create(&log, &[], "x", "x0");
    let l1 = replace(&log, &p, "x", "x0", "x1");
    let l2 = create(&log, &[&l1], "pad2", "p2");
    let l3 = create(&log, &[&l2], "pad3", "p3");
    let s = create(&log, &[&p], "short", "s0");
    let m = merge(&log, &l3, &s);

    let old = legacy_forward(&log, &m);
    assert!(!is_topological(&old), "premise: the BFS order put P after its descendant l1");
    assert_eq!(replay(&old).get("x").map(String::as_str), Some("x0"), "the old replay lost l1's change");

    let new = log.walk_forward(&m, None).unwrap();
    assert!(is_topological(&new), "{:?}", ids(&new));
    assert_eq!(new.len(), old.len());
    assert_eq!(new.last().unwrap().op_id, m, "the head is last");
    assert_eq!(replay(&new).get("x").map(String::as_str), Some("x1"));
}

/// Deterministic pseudo-random DAGs (LCG): 1-parent ops and 2-parent merges.
fn random_dag(seed: u64, n: usize, log: &OpLog) -> Vec<OpId> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut next = move |m: usize| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % m
    };
    let mut ops: Vec<OpId> = Vec::new();
    for i in 0..n {
        let id = if ops.is_empty() {
            create(log, &[], "s0", &format!("g{seed}"))
        } else if ops.len() >= 2 && next(4) == 0 {
            let a = next(ops.len());
            let mut b = next(ops.len());
            if a == b {
                b = (b + 1) % ops.len();
            }
            merge(log, &ops[a], &ops[b])
        } else {
            let p = next(ops.len());
            let sig = format!("s{}", next(4));
            if next(2) == 0 {
                create(log, &[&ops[p]], &sig, &format!("v{seed}_{i}"))
            } else {
                replace(log, &ops[p], &sig, "old", &format!("v{seed}_{i}"))
            }
        };
        ops.push(id);
    }
    ops
}

#[test]
fn walk_forward_is_topological_and_matches_the_old_order_whenever_that_was_valid() {
    let mut differed = 0;
    let mut same = 0;
    for seed in 0..25u64 {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let ops = random_dag(seed, 30, &log);
        for head in ops.iter().skip(3) {
            let new = log.walk_forward(head, None).unwrap();
            let old = legacy_forward(&log, head);
            assert!(is_topological(&new), "seed {seed}: not topological");
            let (mut a, mut b) = (ids(&new), ids(&old));
            a.sort();
            b.sort();
            assert_eq!(a, b, "seed {seed}: the same set of ops");
            assert_eq!(new.last().unwrap().op_id, *head, "the head comes last");
            if is_topological(&old) {
                assert_eq!(ids(&new), ids(&old), "seed {seed}: a valid old order must be kept exactly");
                same += 1;
            } else {
                // The tie-break is pinned: earliest ready op in the old order.
                let want = reference_linearization(&old);
                assert_eq!(new.iter().map(|r| r.op_id.clone()).collect::<Vec<_>>(), want, "seed {seed}: tie-break");
                differed += 1;
            }
            // Idempotent.
            assert_eq!(ids(&OpLog::linearize(new.clone())), ids(&new));
        }
    }
    assert!(differed > 0, "the generator never produced a non-topological BFS order");
    assert!(same > 0);
}

#[test]
fn continues_from_is_exactly_the_pure_continuation() {
    let mut pure = 0;
    let mut leaked = 0;
    for seed in 0..20u64 {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let ops = random_dag(seed, 24, &log);
        for head in ops.iter().skip(2) {
            let ancestors: BTreeSet<OpId> =
                log.walk_back(head, None).unwrap().into_iter().map(|r| r.op_id).collect();
            for since in ops.iter().filter(|o| ancestors.contains(*o) && *o != head) {
                let since_anc: BTreeSet<OpId> =
                    log.walk_back(since, None).unwrap().into_iter().map(|r| r.op_id).collect();
                let exact: BTreeSet<&OpId> = ancestors.difference(&since_anc).collect();
                let got = log.walk_forward_since(head, since).unwrap().expect("since is an ancestor");
                let got_ids: BTreeSet<&OpId> = got.iter().map(|r| &r.op_id).collect();
                let is_pure = got_ids == exact;
                assert_eq!(
                    OpLog::continues_from(&got, since),
                    is_pure,
                    "seed {seed}: continues_from disagrees with the set difference"
                );
                if is_pure {
                    pure += 1;
                } else {
                    leaked += 1;
                }
            }
        }
    }
    assert!(pure > 0 && leaked > 0, "both shapes must occur: pure={pure} leaked={leaked}");
}
