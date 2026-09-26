//! #1062: the head a merge op produces is a function of the resolved merge,
//! not of the order the DAG happens to be replayed in.
//!
//! A merge op is replayed by re-applying both parents' ancestries and then its
//! own `entries`. The two sides are parallel, so their ops on one sig do not
//! commute (`Remove` vs `Replace`, `Replace` vs `Replace`, `Create` vs
//! `Create`): whichever the replay applies last wins. Only sigs listed in
//! `entries` were immune, and the merge listed only sigs whose value differed
//! from dst's, so a `take_ours` (dst already has that value) pinned nothing
//! and the outcome was down to op-id-dependent BFS order.
//!
//! Op ids depend on content only (no timestamps) once the intent is fixed, so
//! instead of hoping a run happens to hit each order, these tests build the
//! DAG by hand and search a salt until the two tips sort the way the case
//! needs — both orders are forced, every time.
//!
//! Each case is checked through every route a head is computed by:
//!   * `sig_map_at_op`            — the canonical full replay;
//!   * `branch_head` on a fresh branch — the full walk, no snapshot;
//!   * `branch_head` after a snapshot at dst — the incremental extension;
//!   * every topological order of the ancestry, replayed by hand — proving the
//!     pinned merge does not depend on the replay order at all.

use std::collections::{BTreeMap, BTreeSet};

use lex_store::{Operation, OperationKind, OperationRecord, StageTransition, Store};
use lex_vcs::{MergeSession, OpLog, Resolution};

type Map = BTreeMap<String, String>;

struct Dag {
    store: Store,
    log: OpLog,
    _tmp: tempfile::TempDir,
    n: usize,
}

impl Dag {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let log = OpLog::open(store.root()).unwrap();
        Dag { store, log, _tmp: tmp, n: 0 }
    }

    fn put(&self, kind: OperationKind, parents: &[&str], t: StageTransition) -> String {
        let rec = OperationRecord::new(
            Operation::new(kind, parents.iter().map(|p| p.to_string()).collect::<Vec<_>>()),
            t,
        );
        self.log.put(&rec).unwrap();
        rec.op_id
    }

    fn add(&self, parent: &[&str], sig: &str, stg: &str) -> String {
        self.put(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stg.into(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            parent,
            StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() },
        )
    }

    fn modify(&self, parent: &str, sig: &str, from: &str, to: &str) -> String {
        self.put(
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

    fn remove(&self, parent: &str, sig: &str, last: &str) -> String {
        self.put(
            OperationKind::RemoveFunction { sig_id: sig.into(), last_stage_id: last.into() },
            &[parent],
            StageTransition::Remove { sig_id: sig.into(), last: last.into() },
        )
    }

    /// A branch that has never been read: its first `branch_head` is a full
    /// walk with no snapshot.
    fn fresh_branch(&mut self, head: &str) -> String {
        self.n += 1;
        let name = format!("probe{}", self.n);
        self.store.advance_branch_head_ff(&name, &head.to_string()).unwrap();
        name
    }

    /// `branch_head` at `head`, computed the way each caller can reach it.
    fn full(&mut self, head: &str) -> Map {
        let b = self.fresh_branch(head);
        self.store.branch_head(&b).unwrap()
    }

    /// Snapshot a branch at `base`, then fast-forward it to `head` and read
    /// the head: the incremental route (or its fallback).
    fn incremental(&mut self, base: &str, head: &str) -> Map {
        let b = self.fresh_branch(base);
        let _ = self.store.branch_head(&b).unwrap();
        self.store.advance_branch_head_ff(&b, &head.to_string()).unwrap();
        self.store.branch_head(&b).unwrap()
    }

    /// Every topological order of `head`'s ancestry, replayed by hand.
    /// Returns the set of distinct resulting maps.
    fn all_orders(&self, head: &str) -> BTreeSet<Vec<(String, String)>> {
        let recs = self.log.walk_back(&head.to_string(), None).unwrap();
        let ids: Vec<String> = recs.iter().map(|r| r.op_id.clone()).collect();
        let idx = |id: &str| ids.iter().position(|x| x == id);
        let n = recs.len();
        let parents: Vec<Vec<usize>> = recs
            .iter()
            .map(|r| r.op.parents.iter().filter_map(|p| idx(p)).collect())
            .collect();
        let mut out = BTreeSet::new();
        let mut placed = vec![false; n];
        let mut map = Map::new();
        fn go(
            recs: &[OperationRecord],
            parents: &[Vec<usize>],
            placed: &mut Vec<bool>,
            map: &mut Map,
            out: &mut BTreeSet<Vec<(String, String)>>,
            left: usize,
        ) {
            if left == 0 {
                out.insert(map.clone().into_iter().collect());
                return;
            }
            for i in 0..recs.len() {
                if placed[i] || !parents[i].iter().all(|&p| placed[p]) {
                    continue;
                }
                let saved = map.clone();
                replay(map, &recs[i].produces);
                placed[i] = true;
                go(recs, parents, placed, map, out, left - 1);
                placed[i] = false;
                *map = saved;
            }
        }
        go(&recs, &parents, &mut placed, &mut map, &mut out, n);
        out
    }
}

fn replay(map: &mut Map, t: &StageTransition) {
    match t {
        StageTransition::Create { sig_id, stage_id } | StageTransition::Replace { sig_id, to: stage_id, .. } => {
            map.insert(sig_id.clone(), stage_id.clone());
        }
        StageTransition::Remove { sig_id, .. } => {
            map.remove(sig_id);
        }
        StageTransition::Rename { from, to, body_stage_id } => {
            map.remove(from);
            map.insert(to.clone(), body_stage_id.clone());
        }
        StageTransition::ImportOnly | StageTransition::FilesOnly => {}
        StageTransition::Merge { entries } => {
            for (s, st) in entries {
                match st {
                    Some(v) => {
                        map.insert(s.clone(), v.clone());
                    }
                    None => {
                        map.remove(s);
                    }
                }
            }
        }
    }
}

// ---- the scenario ---------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    ModifyModify,
    DeleteModify,
    ModifyDelete,
    AddAdd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Res {
    TakeOurs,
    TakeTheirs,
    Custom,
}

/// Which parent id sorts first: `Operation::new` sorts a merge's parents, so
/// this is the one thing that varies between the two replay orders.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Order {
    DstSortsFirst,
    SrcSortsFirst,
}

struct Built {
    dag: Dag,
    dst: String,
    src: String,
    x: &'static str,
}

/// Base `{x?, keep, sm, gone}`; dst modifies `keep`, removes `gone`, and does
/// its side of the conflict on `x`; src adds `other`, modifies `sm`, and does
/// its side. Every branch line is two ops long (`salt` goes in the first),
/// so the two lines are the same length and only the tip ids vary.
fn build(kind: Kind, salt_d: u32, salt_s: u32) -> Built {
    let dag = Dag::new();
    let x = "x";
    let mut base = dag.add(&[], "keep", "k0");
    base = dag.add(&[&base], "sm", "sm0");
    base = dag.add(&[&base], "gone", "g0");
    if kind != Kind::AddAdd {
        base = dag.add(&[&base], x, "x0");
    }
    // dst line.
    let d1 = dag.add(&[&base], "dst-extra", &format!("dx{salt_d}"));
    let d2 = match kind {
        Kind::ModifyModify | Kind::DeleteModify => dag.modify(&d1, x, "x0", "xd"),
        Kind::ModifyDelete => dag.remove(&d1, x, "x0"),
        Kind::AddAdd => dag.add(&[&d1], x, "xd"),
    };
    let d3 = dag.modify(&d2, "keep", "k0", "k1");
    let dst = dag.remove(&d3, "gone", "g0");
    // src line.
    let s1 = dag.add(&[&base], "src-extra", &format!("sx{salt_s}"));
    let s2 = match kind {
        Kind::ModifyModify | Kind::ModifyDelete => dag.modify(&s1, x, "x0", "xs"),
        Kind::DeleteModify => dag.remove(&s1, x, "x0"),
        Kind::AddAdd => dag.add(&[&s1], x, "xs"),
    };
    let s3 = dag.add(&[&s2], "other", "o0");
    let src = dag.modify(&s3, "sm", "sm0", "sm1");
    Built { dag, dst, src, x }
}

fn find(kind: Kind, order: Order) -> Built {
    for salt in 0..400u32 {
        let b = build(kind, salt, 7);
        // The merge's parents are `[min(dst, src), max(dst, src)]`.
        let dst_first = b.dst < b.src;
        if dst_first == (order == Order::DstSortsFirst) {
            return b;
        }
    }
    panic!("no salt gave {order:?} for {kind:?}");
}

/// Merge `src` into `dst` the way the CLI/HTTP commit does: a merge session,
/// resolutions, the production entry builder, then the op landed on a branch.
/// Returns `(merge op id, dst branch name)`. `pinned = false` reproduces the
/// pre-fix entries (only sigs whose value differs from dst) for old-log tests.
fn do_merge(b: &mut Built, res: Res, pinned: bool) -> String {
    let mut session = MergeSession::start("m", &b.dag.log, Some(&b.src), Some(&b.dst)).unwrap();
    let conflicts: Vec<_> = session.remaining_conflicts().into_iter().cloned().collect();
    assert_eq!(conflicts.len(), 1, "one conflict expected, got {conflicts:?}");
    let c = &conflicts[0];
    assert_eq!(c.sig_id, b.x);
    let resolution = match res {
        Res::TakeOurs => Resolution::TakeOurs,
        Res::TakeTheirs => Resolution::TakeTheirs,
        Res::Custom => Resolution::Custom {
            op: Operation::new(
                OperationKind::ModifyBody {
                    sig_id: b.x.into(),
                    from_stage_id: "x0".into(),
                    to_stage_id: "xc".into(),
                    from_budget: None,
                    to_budget: None,
                    to_sig_id: None,
                },
                [b.dst.clone(), b.src.clone()],
            ),
        },
    };
    let verdicts = session.resolve(vec![(c.conflict_id.clone(), resolution)]);
    assert!(verdicts[0].accepted, "{verdicts:?}");
    let auto = session.auto_resolved.clone();
    let out = session.commit().unwrap();

    let mut entries = if pinned {
        b.dag
            .store
            .merge_pins(Some(&b.dst), Some(&b.src), &auto, &out.resolved)
            .unwrap()
    } else {
        // What every commit path built before #1062.
        let mut e = BTreeMap::new();
        for o in &auto {
            if let lex_vcs::MergeOutcome::Src { sig_id, stage_id } = o {
                e.insert(sig_id.clone(), stage_id.clone());
            }
        }
        let src_map = b.dag.store.sig_map_at_op(&b.src).unwrap();
        for (id, r) in &out.resolved {
            match r {
                Resolution::TakeTheirs => {
                    e.insert(id.clone(), src_map.get(id).cloned());
                }
                Resolution::TakeOurs | Resolution::Defer | Resolution::Custom { .. } => {}
            }
        }
        e
    };
    for (id, r) in &out.resolved {
        if let Resolution::Custom { op } = r {
            let (sig, stage) = op.kind.merge_target().unwrap();
            assert_eq!(&sig, id);
            entries.insert(id.clone(), stage);
        }
    }
    let mut parents = [b.dst.clone(), b.src.clone()];
    parents.sort();
    let rec = OperationRecord::new(
        Operation::new(OperationKind::Merge { resolved: entries.len() }, parents),
        StageTransition::Merge { entries },
    );
    b.dag.log.put(&rec).unwrap();
    rec.op_id
}

fn expected(kind: Kind, res: Res) -> Map {
    let mut m: Map = [("keep", "k1"), ("sm", "sm1"), ("other", "o0"), ("dst-extra", ""), ("src-extra", "")]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    // The dummy salt sigs carry the salt in their stage id; drop them from the
    // comparison (see `strip`).
    m.remove("dst-extra");
    m.remove("src-extra");
    let x: Option<&str> = match (kind, res) {
        (_, Res::Custom) => Some("xc"),
        (Kind::ModifyModify, Res::TakeOurs) | (Kind::DeleteModify, Res::TakeOurs) | (Kind::AddAdd, Res::TakeOurs) => Some("xd"),
        (Kind::ModifyDelete, Res::TakeOurs) => None,
        (Kind::ModifyModify, Res::TakeTheirs) | (Kind::ModifyDelete, Res::TakeTheirs) | (Kind::AddAdd, Res::TakeTheirs) => Some("xs"),
        (Kind::DeleteModify, Res::TakeTheirs) => None,
    };
    if let Some(v) = x {
        m.insert("x".into(), v.into());
    }
    m
}

fn strip(mut m: Map) -> Map {
    m.remove("dst-extra");
    m.remove("src-extra");
    m
}

const KINDS: [Kind; 4] = [Kind::ModifyModify, Kind::DeleteModify, Kind::ModifyDelete, Kind::AddAdd];
const RESS: [Res; 3] = [Res::TakeOurs, Res::TakeTheirs, Res::Custom];
const ORDERS: [Order; 2] = [Order::DstSortsFirst, Order::SrcSortsFirst];

#[test]
fn every_kind_and_resolution_yields_the_resolved_head_under_both_replay_orders() {
    for kind in KINDS {
        for res in RESS {
            for order in ORDERS {
                let mut b = find(kind, order);
                let m = do_merge(&mut b, res, true);
                let want = expected(kind, res);
                let ctx = format!("{kind:?} / {res:?} / {order:?}");

                assert_eq!(strip(b.dag.store.sig_map_at_op(&m).unwrap()), want, "sig_map_at_op: {ctx}");
                assert_eq!(strip(b.dag.full(&m)), want, "full walk: {ctx}");
                let dst = b.dst.clone();
                assert_eq!(strip(b.dag.incremental(&dst, &m)), want, "incremental from dst: {ctx}");
                let src = b.src.clone();
                assert_eq!(strip(b.dag.incremental(&src, &m)), want, "incremental from src: {ctx}");

                // Order-independence proper: EVERY topological order agrees.
                let all: BTreeSet<Map> = b
                    .dag
                    .all_orders(&m)
                    .into_iter()
                    .map(|v| strip(v.into_iter().collect()))
                    .collect();
                assert_eq!(all.len(), 1, "replay-order dependent ({} distinct heads): {ctx}: {all:?}", all.len());
                assert_eq!(all.iter().next().unwrap(), &want, "all orders: {ctx}");
            }
        }
    }
}

/// The reported bug, as a table: which (kind, resolution) pairs a replay
/// order could break when the merge lists only what differs from dst — i.e.
/// what every merge written before this fix looks like. Documents the whole
/// affected set (the issue only saw `delete_modify` + `take_ours`).
#[test]
fn legacy_unpinned_merges_are_order_dependent_exactly_for_take_ours_shaped_resolutions() {
    let mut dependent = Vec::new();
    for kind in KINDS {
        for res in RESS {
            let mut b = find(kind, Order::DstSortsFirst);
            let m = do_merge(&mut b, res, false);
            let heads: BTreeSet<Map> = b
                .dag
                .all_orders(&m)
                .into_iter()
                .map(|v| strip(v.into_iter().collect()))
                .collect();
            if heads.len() > 1 {
                dependent.push((kind, res));
            }
        }
    }
    let want: Vec<(Kind, Res)> = KINDS.iter().map(|k| (*k, Res::TakeOurs)).collect();
    assert_eq!(dependent, want, "order-dependent unpinned merges: {dependent:?}");
}

/// Old merge ops already in a log keep loading, and replay to ONE answer,
/// identical from every route. Where the pre-fix walk was already a valid
/// topological order (same-length lines, as here) that answer is exactly the
/// one the pre-fix code computed by full replay: oracle below.
#[test]
fn legacy_unpinned_merges_replay_to_the_answer_the_old_walk_gave() {
    fn legacy_walk_forward(log: &OpLog, head: &str) -> Vec<OperationRecord> {
        // The pre-#1062 `walk_forward`: BFS from the head, reversed.
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut frontier: std::collections::VecDeque<String> = [head.to_string()].into();
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
    for kind in KINDS {
        for res in RESS {
            for order in ORDERS {
                let mut b = find(kind, order);
                let m = do_merge(&mut b, res, false);
                let ctx = format!("{kind:?} / {res:?} / {order:?}");
                let mut old = Map::new();
                for r in legacy_walk_forward(&b.dag.log, &m) {
                    replay(&mut old, &r.produces);
                }
                let new = b.dag.store.sig_map_at_op(&m).unwrap();
                assert_eq!(new, old, "old-log head changed: {ctx}");
                assert_eq!(b.dag.full(&m), old, "full walk: {ctx}");
                let dst = b.dst.clone();
                assert_eq!(b.dag.incremental(&dst, &m), old, "incremental: {ctx}");
            }
        }
    }
}

// ---- manifests ------------------------------------------------------------

#[test]
fn files_manifest_of_a_merge_does_not_depend_on_parent_order() {
    use lex_store::files::ManifestAt;
    for order in ORDERS {
        let mut b = find(Kind::ModifyModify, order);
        // Both lines carry a `SetFiles`: same manifest -> that manifest;
        // different manifests -> Ambiguous. Neither may depend on which parent
        // sorts first.
        for (md, ms, want) in [
            ("blob-a", "blob-a", ManifestAt::Set { manifest: "blob-a".into() }),
            ("blob-a", "blob-b", ManifestAt::Ambiguous),
        ] {
            let sf = |parent: &str, m: &str| {
                b.dag.put(
                    OperationKind::SetFiles { manifest: m.into() },
                    &[parent],
                    StageTransition::FilesOnly,
                )
            };
            let d = sf(&b.dst, md);
            let s = sf(&b.src, ms);
            let mut parents = [d.clone(), s.clone()];
            parents.sort();
            let m = b.dag.put(
                OperationKind::Merge { resolved: 0 },
                &[&parents[0], &parents[1]],
                StageTransition::Merge { entries: BTreeMap::new() },
            );
            assert_eq!(b.dag.store.manifest_at(&m).unwrap(), want, "{order:?} {md}/{ms}");
            let br = b.dag.fresh_branch(&m);
            assert_eq!(b.dag.store.branch_manifest(&br).unwrap(), want, "{order:?} {md}/{ms} (branch)");
            // Incremental: snapshot at dst's SetFiles, then advance to the merge.
            let br2 = b.dag.fresh_branch(&d);
            let _ = b.dag.store.branch_manifest(&br2).unwrap();
            b.dag.store.advance_branch_head_ff(&br2, &m).unwrap();
            assert_eq!(b.dag.store.branch_manifest(&br2).unwrap(), want, "{order:?} {md}/{ms} (incremental)");
        }
    }
}

// ---- composing merges -----------------------------------------------------

/// A merge of a branch that already contains a merge: the second merge's pins
/// compose with the first's, and the result is again independent of order.
#[test]
fn a_merge_on_top_of_a_merge_composes() {
    let mut b = find(Kind::ModifyModify, Order::DstSortsFirst);
    let m1 = do_merge(&mut b, Res::TakeOurs, true);
    // Continue both lines: dst (now m1) modifies `x` again; a src-side
    // descendant modifies it a different way — a fresh conflict against a
    // history that already contains a merge.
    let d = b.dag.modify(&m1, "x", "xd", "xd2");
    let s = b.dag.modify(&b.src.clone(), "x", "xs", "xs2");
    let mut b2 = Built { dag: b.dag, dst: d, src: s, x: "x" };
    let m2 = {
        // lca of d and s is `src` (s descends from it; d descends from m1
        // which has `src` as a parent).
        let session = MergeSession::start("m2", &b2.dag.log, Some(&b2.src), Some(&b2.dst)).unwrap();
        let conflicts: Vec<_> = session.remaining_conflicts().into_iter().cloned().collect();
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        let mut session = session;
        session.resolve(vec![(conflicts[0].conflict_id.clone(), Resolution::TakeOurs)]);
        let auto = session.auto_resolved.clone();
        let out = session.commit().unwrap();
        let entries = b2
            .dag
            .store
            .merge_pins(Some(&b2.dst), Some(&b2.src), &auto, &out.resolved)
            .unwrap();
        let mut parents = [b2.dst.clone(), b2.src.clone()];
        parents.sort();
        let rec = OperationRecord::new(
            Operation::new(OperationKind::Merge { resolved: entries.len() }, parents),
            StageTransition::Merge { entries },
        );
        b2.dag.log.put(&rec).unwrap();
        rec.op_id
    };
    let mut want = strip(b2.dag.store.sig_map_at_op(&m2).unwrap());
    assert_eq!(want.get("x").map(String::as_str), Some("xd2"), "take_ours keeps dst's latest: {want:?}");
    assert_eq!(want.remove("keep").as_deref(), Some("k1"));
    let all: BTreeSet<Map> = b2.dag.all_orders(&m2).into_iter().map(|v| strip(v.into_iter().collect())).collect();
    assert_eq!(all.len(), 1, "second merge is order dependent: {all:?}");
    let dst = b2.dst.clone();
    assert_eq!(strip(b2.dag.full(&m2)), strip(b2.dag.store.sig_map_at_op(&m2).unwrap()));
    assert_eq!(strip(b2.dag.incremental(&dst, &m2)), strip(b2.dag.store.sig_map_at_op(&m2).unwrap()));
}

/// Two merges of the same pair of branches in opposite directions, then a
/// merge of those: the criss-cross the LCA code documents as unresolved. The
/// pins still make the result a function of the merge.
#[test]
fn criss_cross_merges_are_order_independent() {
    let mut b = find(Kind::ModifyModify, Order::SrcSortsFirst);
    // m1: src into dst (take_ours -> x = xd).
    let m1 = do_merge(&mut b, Res::TakeOurs, true);
    // m2: dst into src the other way round (take_ours there = src's x = xs).
    let mut b_rev = Built { dag: b.dag, dst: b.src.clone(), src: b.dst.clone(), x: "x" };
    let m2 = do_merge(&mut b_rev, Res::TakeOurs, true);
    let mut dag = b_rev.dag;
    // m3: merge the two merges, tolerating whatever the engine reports.
    let session = MergeSession::start("m3", &dag.log, Some(&m2), Some(&m1)).unwrap();
    let conflicts: Vec<_> = session.remaining_conflicts().into_iter().cloned().collect();
    let mut session = session;
    let rs: Vec<_> = conflicts.iter().map(|c| (c.conflict_id.clone(), Resolution::TakeOurs)).collect();
    session.resolve(rs);
    let auto = session.auto_resolved.clone();
    let out = session.commit().unwrap();
    let entries = dag.store.merge_pins(Some(&m1), Some(&m2), &auto, &out.resolved).unwrap();
    let mut parents = [m1.clone(), m2.clone()];
    parents.sort();
    let rec = OperationRecord::new(
        Operation::new(OperationKind::Merge { resolved: entries.len() }, parents),
        StageTransition::Merge { entries },
    );
    dag.log.put(&rec).unwrap();
    let m3 = rec.op_id;
    let all: BTreeSet<Map> = dag.all_orders(&m3).into_iter().map(|v| strip(v.into_iter().collect())).collect();
    assert_eq!(all.len(), 1, "criss-cross merge is order dependent: {all:?}");
    let got = strip(dag.store.sig_map_at_op(&m3).unwrap());
    assert_eq!(all.iter().next().unwrap(), &got);
    assert_eq!(strip(dag.full(&m3)), got);
    assert_eq!(strip(dag.incremental(&m1, &m3)), got);
    // dst of m3 is m1, whose take_ours pinned x = xd.
    assert_eq!(got.get("x").map(String::as_str), Some("xd"), "{got:?}");
}

// ---- incremental snapshot == full replay ----------------------------------

/// The incremental `HeadSnapshot` extension must agree with a full replay for
/// EVERY (ancestor, head) pair — including heads that are merges, where it
/// used to re-apply the merged-in branch's pre-fork history over the snapshot
/// and silently revert dst's own changes (the `keep` sig in the scenarios
/// above is exactly that; this is the exhaustive form).
#[test]
fn incremental_snapshot_equals_full_replay_on_random_merge_dags() {
    let mut merges_checked = 0;
    for seed in 0..6u64 {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = move |m: usize| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % m
        };
        let mut dag = Dag::new();
        let sigs = ["a", "b", "c", "d"];
        let mut ops: Vec<String> = vec![dag.add(&[], "a", "a0")];
        for i in 1..18 {
            let id = if ops.len() >= 2 && next(4) == 0 {
                let a = next(ops.len());
                let mut b = next(ops.len());
                if a == b {
                    b = (b + 1) % ops.len();
                }
                let mut entries = BTreeMap::new();
                for s in sigs {
                    match next(4) {
                        0 => {
                            entries.insert(s.to_string(), Some(format!("m{seed}_{i}_{s}")));
                        }
                        1 => {
                            entries.insert(s.to_string(), None);
                        }
                        _ => {}
                    }
                }
                let mut parents = [ops[a].clone(), ops[b].clone()];
                parents.sort();
                dag.put(
                    OperationKind::Merge { resolved: entries.len() },
                    &[&parents[0], &parents[1]],
                    StageTransition::Merge { entries },
                )
            } else {
                let p = ops[next(ops.len())].clone();
                let sig = sigs[next(4)];
                match next(3) {
                    0 => dag.add(&[&p], sig, &format!("c{seed}_{i}")),
                    1 => dag.modify(&p, sig, "old", &format!("r{seed}_{i}")),
                    _ => dag.remove(&p, sig, "old"),
                }
            };
            ops.push(id);
        }
        for head in ops.clone() {
            let anc: BTreeSet<String> =
                dag.log.walk_back(&head, None).unwrap().into_iter().map(|r| r.op_id).collect();
            let truth = dag.store.sig_map_at_op(&head).unwrap();
            assert_eq!(dag.full(&head), truth, "seed {seed}: full walk != sig_map_at_op");
            let is_merge = dag.log.get(&head).unwrap().unwrap().op.parents.len() == 2;
            for base in ops.iter().filter(|o| anc.contains(*o) && **o != head).take(4) {
                assert_eq!(
                    dag.incremental(base, &head),
                    truth,
                    "seed {seed}: incremental {} -> {} differs from a full replay",
                    &base[..8],
                    &head[..8]
                );
                if is_merge {
                    merges_checked += 1;
                }
            }
        }
    }
    assert!(merges_checked > 20, "too few merge heads exercised: {merges_checked}");
}

/// The plainest form of the snapshot bug, with no conflict at all: dst
/// modifies `keep`, src adds `other`, and a merge lists only `other` (which
/// is all any merge op written before #1062 lists for a dst-only change).
/// Extending dst's snapshot with `walk_forward_since` used to re-apply src's
/// whole ancestry — including the pre-fork `Create keep = k0` — on top of it,
/// so the head came back with `keep` reverted to `k0` while a fresh replay (a
/// pulled store, `export-git`) had `k1`.
#[test]
fn a_merge_does_not_revert_a_dst_only_change_through_the_snapshot() {
    let mut dag = Dag::new();
    let root = dag.add(&[], "keep", "k0");
    let d = dag.modify(&root, "keep", "k0", "k1");
    let s = dag.add(&[&root], "other", "o0");
    let mut parents = [d.clone(), s.clone()];
    parents.sort();
    let m = dag.put(
        OperationKind::Merge { resolved: 1 },
        &[&parents[0], &parents[1]],
        StageTransition::Merge { entries: [("other".to_string(), Some("o0".to_string()))].into() },
    );
    let want: Map = [("keep", "k1"), ("other", "o0")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    assert_eq!(dag.full(&m), want, "full walk");
    assert_eq!(dag.incremental(&d, &m), want, "extending dst's snapshot must not revert `keep`");
}
