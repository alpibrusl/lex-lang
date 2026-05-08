//! Property tests for the four canonical-form invariants.
//!
//! Each test generates a deterministic corpus of cases — no RNG, no
//! `proptest`, no flaky CI — and verifies the invariant holds across
//! every case. Total case count exceeds 1000 per test; the corpus
//! sizes are sized so that `cargo test -p lex-vcs --test
//! opid_property` finishes in well under a second.
//!
//! The four invariants:
//!
//! 1. **Round-trip stability.** `op = deserialize(serialize(op))`
//!    yields the same `OpId`.
//! 2. **Parent-order independence.** Permuting the order in which
//!    parents are passed to `Operation::new` yields the same
//!    `OpId`.
//! 3. **Effect-set order independence.** Effects are a
//!    `BTreeSet<String>`; insertion order through any iteration
//!    order yields the same canonical form.
//! 4. **Merge-entry order independence.** `StageTransition::Merge`
//!    entries are a `BTreeMap<SigId, _>`; insertion order through
//!    any iteration order yields the same canonical bytes.

use lex_vcs::{
    EffectSet, Operation, OperationKind, OperationRecord, SigId, StageId, StageTransition,
};
use std::collections::{BTreeMap, BTreeSet};

// -- corpus builders ------------------------------------------------

/// Construct a generous variety of `OperationKind` values covering
/// every variant. Sized large enough that the per-test case count
/// crosses 1000 once we multiply by the inner permutation count.
fn corpus() -> Vec<OperationKind> {
    let mut out = Vec::new();

    let sigs = ["fac::Int->Int", "parse::Str->Int", "Color", "User", "writer"];
    let stages = ["s0", "s1", "stage-abc-def", "deadbeef", "0"];
    let effect_sets: Vec<EffectSet> = vec![
        BTreeSet::new(),
        ["io".into()].into_iter().collect(),
        ["fs_write".into(), "io".into()].into_iter().collect(),
        ["net(\"wttr.in\")".into()].into_iter().collect(),
        ["net".into(), "net(\"a\")".into(), "net(\"b\")".into()].into_iter().collect(),
    ];

    for sig in sigs {
        for stage in stages {
            for eff in &effect_sets {
                out.push(OperationKind::AddFunction {
                    sig_id: sig.into(),
                    stage_id: stage.into(),
                    effects: eff.clone(),
                });
            }
            out.push(OperationKind::RemoveFunction {
                sig_id: sig.into(),
                last_stage_id: stage.into(),
            });
            out.push(OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: stage.into(),
                to_stage_id: format!("{stage}-next"),
            });
            out.push(OperationKind::AddType {
                sig_id: sig.into(),
                stage_id: stage.into(),
            });
            out.push(OperationKind::RemoveType {
                sig_id: sig.into(),
                last_stage_id: stage.into(),
            });
            out.push(OperationKind::ModifyType {
                sig_id: sig.into(),
                from_stage_id: stage.into(),
                to_stage_id: format!("{stage}-next"),
            });
        }
    }

    for from in sigs {
        for to in sigs {
            if from == to { continue; }
            for stage in stages {
                out.push(OperationKind::RenameSymbol {
                    from: from.into(),
                    to: to.into(),
                    body_stage_id: stage.into(),
                });
            }
        }
    }

    for sig in sigs {
        for from_eff in &effect_sets {
            for to_eff in &effect_sets {
                if from_eff == to_eff { continue; }
                out.push(OperationKind::ChangeEffectSig {
                    sig_id: sig.into(),
                    from_stage_id: "s-old".into(),
                    to_stage_id: "s-new".into(),
                    from_effects: from_eff.clone(),
                    to_effects: to_eff.clone(),
                });
            }
        }
    }

    for file in ["src/main.lex", "lib/util.lex", "deeply/nested/path.lex"] {
        for module in ["std.io", "std.parser", "./local", "../sibling/m"] {
            out.push(OperationKind::AddImport {
                in_file: file.into(),
                module: module.into(),
            });
            out.push(OperationKind::RemoveImport {
                in_file: file.into(),
                module: module.into(),
            });
        }
    }

    for resolved in [0usize, 1, 5, 50, 1000] {
        out.push(OperationKind::Merge { resolved });
    }

    out
}

/// Deterministic permutations of a slice. Generates `n!`-bounded
/// variants by cyclic rotation + reverse + index shuffles, enough
/// to expose any order-sensitivity in the canonical form without
/// pulling in an RNG.
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    let mut out = vec![items.to_vec()];
    for k in 1..items.len() {
        let mut rotated = items.to_vec();
        rotated.rotate_left(k);
        out.push(rotated);
    }
    let mut reversed = items.to_vec();
    reversed.reverse();
    out.push(reversed);
    if items.len() >= 2 {
        let mut swap_first_last = items.to_vec();
        let last = swap_first_last.len() - 1;
        swap_first_last.swap(0, last);
        out.push(swap_first_last);
    }
    out
}

// -- invariant 1: round-trip stability ------------------------------

#[test]
fn op_id_round_trips_through_serde_json() {
    let mut cases = 0usize;
    let parent_sets: &[&[&str]] = &[
        &[],
        &["op-a"],
        &["op-a", "op-b"],
        &["op-a", "op-b", "op-c"],
    ];
    let intents: &[Option<&str>] = &[None, Some("intent-a"), Some("intent-b")];

    for kind in corpus() {
        for parents in parent_sets {
            for intent in intents {
                let parents_owned: Vec<String> =
                    parents.iter().map(|s| (*s).to_string()).collect();
                let mut op = Operation::new(kind.clone(), parents_owned);
                if let Some(i) = *intent {
                    op = op.with_intent(i);
                }
                let json = serde_json::to_string(&op).expect("serialize");
                let back: Operation = serde_json::from_str(&json).expect("deserialize");
                assert_eq!(
                    op.op_id(),
                    back.op_id(),
                    "round-trip changed op_id on kind {:?}",
                    kind,
                );
                assert_eq!(op, back);
                cases += 1;
            }
        }
    }
    assert!(cases >= 1000, "round-trip corpus too small: {cases}");
}

// -- invariant 2: parent-order independence -------------------------

#[test]
fn op_id_is_independent_of_parent_order() {
    let parent_sets: &[Vec<&str>] = &[
        vec!["op-a", "op-b"],
        vec!["op-a", "op-b", "op-c"],
        vec!["op-a", "op-b", "op-c", "op-d"],
        // duplicates should also collapse to the same op_id; the
        // dedup step in Operation::new is part of the canonical
        // contract.
        vec!["op-a", "op-a", "op-b"],
        vec!["op-a", "op-b", "op-b", "op-c"],
    ];
    let mut cases = 0usize;
    for kind in corpus() {
        for parents in parent_sets {
            let parents_strs: Vec<String> = parents.iter().map(|s| (*s).to_string()).collect();
            let baseline = Operation::new(kind.clone(), parents_strs.clone()).op_id();
            for permuted in permutations(&parents_strs) {
                let live = Operation::new(kind.clone(), permuted.clone()).op_id();
                assert_eq!(
                    baseline, live,
                    "parent permutation {:?} of {:?} drifted the op_id",
                    permuted, parents_strs,
                );
                cases += 1;
            }
        }
    }
    assert!(cases >= 1000, "parent-order corpus too small: {cases}");
}

// -- invariant 3: effect-set order independence ---------------------

#[test]
fn op_id_is_independent_of_effect_insertion_order() {
    let effect_pool: &[&str] = &[
        "io", "fs_write", "fs_read", "net", "net(\"a.com\")",
        "net(\"b.com\")", "mcp", "mcp(\"ocpp\")", "llm_local", "llm_cloud",
    ];

    fn build_op(effects: EffectSet) -> Operation {
        Operation::new(
            OperationKind::AddFunction {
                sig_id: "f".into(),
                stage_id: "s".into(),
                effects,
            },
            ["op-parent".into()],
        )
    }

    let mut cases = 0usize;
    // All non-empty subsets of size 2..=5 from the pool.
    for size in 2..=5 {
        for combo in combinations(effect_pool, size) {
            let baseline_set: EffectSet = combo.iter().map(|s| s.to_string()).collect();
            let baseline = build_op(baseline_set).op_id();
            for permuted in permutations(&combo) {
                // Construct the BTreeSet from the permuted iter. The
                // BTreeSet's structural sort means the resulting
                // effect set is identical, so op_id should match.
                let permuted_set: EffectSet =
                    permuted.iter().map(|s| s.to_string()).collect();
                let live = build_op(permuted_set).op_id();
                assert_eq!(
                    baseline, live,
                    "effect insertion order {:?} drifted op_id",
                    permuted,
                );
                cases += 1;
            }
        }
    }
    assert!(cases >= 1000, "effect-order corpus too small: {cases}");
}

// -- invariant 4: merge-entry order independence --------------------

#[test]
fn merge_transition_canonical_bytes_independent_of_insertion_order() {
    // StageTransition::Merge.entries is BTreeMap<SigId, _>, so the
    // canonical-form invariant is structurally enforced by the type:
    // any caller that inserts in any order produces the same
    // BTreeMap, which serializes the same way. Test it explicitly so
    // a future swap to HashMap fails loudly.

    let pool: &[(&str, Option<&str>)] = &[
        ("sig-a", Some("stage-a")),
        ("sig-b", None),
        ("sig-c", Some("stage-c")),
        ("sig-d", Some("stage-d")),
        ("sig-e", None),
        ("sig-f", Some("stage-f")),
        ("sig-g", Some("stage-g")),
        ("sig-h", None),
        ("sig-i", Some("stage-i")),
        ("sig-j", Some("stage-j")),
    ];

    let mut cases = 0usize;
    // Enumerating subsets of every size in 2..=pool.len() blows up
    // (2^10 = 1024 base subsets) — clamp to size <= 6 to keep
    // runtime under a second; that still produces > 1000 cases
    // after the permutation fan-out.
    for size in 2..=6 {
        for combo in combinations_owned::<(&str, Option<&str>)>(pool, size) {
            let baseline_map: BTreeMap<SigId, Option<StageId>> = combo
                .iter()
                .map(|(s, st)| (s.to_string(), st.map(|x| x.to_string())))
                .collect();
            let baseline_t = StageTransition::Merge { entries: baseline_map };
            let baseline_bytes = serde_json::to_vec(&baseline_t).expect("serialize");

            for permuted in permutations(&combo) {
                let map: BTreeMap<SigId, Option<StageId>> = permuted
                    .iter()
                    .map(|(s, st)| (s.to_string(), st.map(|x| x.to_string())))
                    .collect();
                let live_t = StageTransition::Merge { entries: map };
                let live_bytes = serde_json::to_vec(&live_t).expect("serialize");
                assert_eq!(
                    baseline_bytes, live_bytes,
                    "merge-entry insertion order {:?} drifted canonical bytes",
                    permuted,
                );
                cases += 1;
            }
        }
    }
    assert!(cases >= 1000, "merge-entry corpus too small: {cases}");
}

// -- bonus invariant: OpId of a Merge op is independent of the produces
// transition entries, because op_id only covers (kind, parents,
// intent_id). Documented in canonical.rs; verified here.

#[test]
fn merge_op_id_is_independent_of_stage_transition_entries() {
    let parents: Vec<String> = vec!["op-a".into(), "op-b".into()];
    let kind = OperationKind::Merge { resolved: 3 };
    let op = Operation::new(kind, parents);
    let baseline = op.op_id();

    let entry_sets: Vec<BTreeMap<SigId, Option<StageId>>> = vec![
        BTreeMap::new(),
        [("sig-a".to_string(), Some("stage-1".to_string()))]
            .into_iter().collect(),
        [
            ("sig-a".to_string(), Some("stage-1".to_string())),
            ("sig-b".to_string(), None),
            ("sig-c".to_string(), Some("stage-99".to_string())),
        ].into_iter().collect(),
    ];

    for entries in entry_sets {
        let rec = OperationRecord::new(op.clone(), StageTransition::Merge { entries });
        assert_eq!(
            rec.op_id, baseline,
            "OperationRecord.op_id drifted when StageTransition entries changed",
        );
    }
}

// -- helpers --------------------------------------------------------

/// All k-combinations of `items` as borrowed slices. Deterministic
/// order; used to keep the corpus generation reproducible without
/// pulling in itertools.
fn combinations<'a>(items: &'a [&'a str], k: usize) -> Vec<Vec<&'a str>> {
    let mut out = Vec::new();
    let n = items.len();
    if k > n { return out; }
    let mut idx = (0..k).collect::<Vec<_>>();
    loop {
        out.push(idx.iter().map(|i| items[*i]).collect());
        let mut i = k;
        while i > 0 {
            i -= 1;
            if idx[i] != i + n - k { break; }
            if i == 0 { return out; }
        }
        idx[i] += 1;
        for j in (i + 1)..k {
            idx[j] = idx[j - 1] + 1;
        }
    }
}

fn combinations_owned<T: Copy>(items: &[T], k: usize) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let n = items.len();
    if k > n { return out; }
    let mut idx = (0..k).collect::<Vec<_>>();
    loop {
        out.push(idx.iter().map(|i| items[*i]).collect());
        let mut i = k;
        while i > 0 {
            i -= 1;
            if idx[i] != i + n - k { break; }
            if i == 0 { return out; }
        }
        idx[i] += 1;
        for j in (i + 1)..k {
            idx[j] = idx[j - 1] + 1;
        }
    }
}
