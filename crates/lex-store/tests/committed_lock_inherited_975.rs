//! #975: a head the **server** creates carries no committed lock of its own —
//! a merge op, or a head landed through `/v1/patch`. Locks are only committed
//! for heads a *client pushes*. An exact-key lookup therefore returned `None`,
//! the dependency resolver got no pins, and a non-inlined head was rejected as
//! `unknown_identifier "<alias>"` even though every dependency was resolvable.
//!
//! `committed_lock_inherited` walks back to the nearest ancestor carrying a
//! lock, so a head inherits its ancestors' pins until a new lock is committed.
//!
//! Found by prod-testing #945 against vcs.lexlang.org: the *pushed* head
//! attested `type_check: passed`, while `POST /v1/merge/<id>/commit` on the
//! same package 422'd with `unknown_identifier "nt"`.

use std::collections::BTreeSet;

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};

const LOCK_A: &str = "version = 1\n\n[[package]]\nname = \"lex-nt\"\nregistry = \"vcs.lexlang.org/lex-official/lex-nt\"\nconstraint = \"^1.0\"\nversion = \"1.0.0\"\nhead_op = \"op_nt\"\n";
const LOCK_B: &str = "version = 1\n\n[[package]]\nname = \"lex-nt\"\nregistry = \"vcs.lexlang.org/lex-official/lex-nt\"\nconstraint = \"^1.0\"\nversion = \"1.0.1\"\nhead_op = \"op_nt2\"\n";

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

/// Land an op on `branch`, ungated — only the op *graph* matters here.
fn land(s: &Store, branch: &str, sig: &str, parents: Vec<String>) -> String {
    let stg = format!("stg-{sig}");
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.to_string(),
            stage_id: stg.clone(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        },
        parents,
    );
    let t = StageTransition::Create { sig_id: sig.to_string(), stage_id: stg };
    s.apply_operation(branch, op, t).expect("land op")
}

#[test]
fn a_head_without_its_own_lock_inherits_its_ancestors() {
    let (store, _tmp) = fresh();
    let a = land(&store, DEFAULT_BRANCH, "a", vec![]);
    store.set_committed_lock(&a, LOCK_A).expect("set lock on a");
    // `b` is the shape of a server-created head: no lock was ever pushed for it.
    let b = land(&store, DEFAULT_BRANCH, "b", vec![a.clone()]);

    // The exact lookup is what broke dependency resolution...
    assert_eq!(store.committed_lock(&b).expect("exact"), None);
    // ...and the inherited lookup is what fixes it.
    assert_eq!(
        store.committed_lock_inherited(&b).expect("inherited").as_deref(),
        Some(LOCK_A),
        "a head with no lock of its own must inherit its ancestor's pins"
    );
}

#[test]
fn the_nearest_ancestor_wins() {
    let (store, _tmp) = fresh();
    let a = land(&store, DEFAULT_BRANCH, "a", vec![]);
    store.set_committed_lock(&a, LOCK_A).expect("set a");
    let b = land(&store, DEFAULT_BRANCH, "b", vec![a.clone()]);
    store.set_committed_lock(&b, LOCK_B).expect("set b");
    let c = land(&store, DEFAULT_BRANCH, "c", vec![b.clone()]);

    assert_eq!(
        store.committed_lock_inherited(&c).expect("inherited").as_deref(),
        Some(LOCK_B),
        "the nearest ancestor's lock must win, not the oldest"
    );
}

#[test]
fn a_head_with_its_own_lock_uses_it() {
    let (store, _tmp) = fresh();
    let a = land(&store, DEFAULT_BRANCH, "a", vec![]);
    store.set_committed_lock(&a, LOCK_A).expect("set a");
    let b = land(&store, DEFAULT_BRANCH, "b", vec![a.clone()]);
    store.set_committed_lock(&b, LOCK_B).expect("set b");

    assert_eq!(
        store.committed_lock_inherited(&b).expect("inherited").as_deref(),
        Some(LOCK_B),
        "a head's own lock must take precedence over any ancestor's"
    );
}

#[test]
fn a_merge_shaped_head_inherits_from_its_first_parent() {
    let (store, _tmp) = fresh();
    // dst carries the lock; src does not — the merge op has neither.
    let dst = land(&store, DEFAULT_BRANCH, "dst", vec![]);
    store.set_committed_lock(&dst, LOCK_A).expect("set dst");
    store.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let src = land(&store, "feature", "src", vec![dst.clone()]);

    // The merge op's parents are [dst, src], exactly as the commit handler builds them.
    let merge = {
        let op = Operation::new(
            OperationKind::Merge { resolved: 0 },
            vec![dst.clone(), src.clone()],
        );
        let t = StageTransition::Merge { entries: Default::default() };
        store.apply_operation(DEFAULT_BRANCH, op, t).expect("land merge")
    };

    assert_eq!(store.committed_lock(&merge).expect("exact"), None);
    assert_eq!(
        store.committed_lock_inherited(&merge).expect("inherited").as_deref(),
        Some(LOCK_A),
        "a merge op must inherit the pins of the branch it merges into"
    );
}

#[test]
fn no_lock_anywhere_in_the_ancestry_is_none_not_an_error() {
    let (store, _tmp) = fresh();
    let a = land(&store, DEFAULT_BRANCH, "a", vec![]);
    let b = land(&store, DEFAULT_BRANCH, "b", vec![a]);
    // A dependency-free package commits no lock at all; that must stay `None`
    // rather than erroring, so the gate simply has nothing to resolve.
    assert_eq!(store.committed_lock_inherited(&b).expect("inherited"), None);
}

#[test]
fn an_unknown_head_terminates_the_walk() {
    let (store, _tmp) = fresh();
    // An incomplete op-log (a partially-synced store) must not fail resolution.
    assert_eq!(store.committed_lock_inherited("op_does_not_exist").expect("inherited"), None);
}
