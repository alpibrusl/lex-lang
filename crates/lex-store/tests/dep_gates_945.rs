//! #945: **every** write-path gate must resolve a non-inlined head's
//! dependencies, not just publish and the hub's `verify_head_and_attest`.
//!
//! #930 made dependencies non-inlined, so a head keeps its `import` edges and
//! the gate resolves each dependency's interface at check time. But three gates
//! still called `resolved_modules(stages, None)` with a head reconstructed from
//! the SigId→stage map — which holds only fn/type declarations, never the
//! head's `AddImport` edges. So the checker saw no `import` to bind the alias
//! to and rejected a perfectly valid head as `unknown_identifier "<alias>"`:
//!
//! 1. `apply_operation_checked` / `apply_operation_gated` — the `/v1/patch` path
//! 2. `apply_merge_op_gated` — the merge-commit gate
//! 3. `typecheck_merge_projection` — the merge resolve-time gate (#834)
//!
//! Each test below lands or checks a head that imports `lex-nt/lib` and calls
//! `nt.gcd`. **Every one fails with `unknown_identifier "nt"` without the fix**
//! — they are mutation-checked, not decoration.
//!
//! Note `publish_program_with_intent` deliberately still passes `None`: it
//! *creates* the head, so no committed lock is keyed to it yet and the pins live
//! in the caller's working-copy lock. See the comment at that call site.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_store::{DepResolver, Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_types::{module_record_from_fields, EffectSet, Ty};
use lex_vcs::{ImportMap, ImportRef};

/// Supplies `lex-nt/lib` with `gcd(Int, Int) -> Int` (plus the `lex-mathx` and
/// `lex-strx` packages the #977 tests add) — but **only for a head whose pins
/// it can actually find**, exactly like the real `HubDepResolver`, which reads
/// the committed lock.
///
/// This lock-awareness is deliberate. A resolver that answers unconditionally
/// hides #975: it would resolve for a merge or `/v1/patch` head that carries no
/// lock, so the gate tests would pass while production failed with
/// `unknown_identifier`. That is precisely what happened — the original mock
/// here was unconditional, and only a live prod merge exposed the gap. Keeping
/// the fixture honest about the lock makes that class of bug catchable here.
///
/// #977 sharpens it further: a package resolves only if the governing lock
/// **pins that package**, not merely if *some* lock governs the head. Otherwise
/// a merge that inherited the wrong parent's lock (one missing a dependency the
/// other branch introduced) would still resolve here and hide the bug.
struct NtResolver {
    root: std::path::PathBuf,
}
impl DepResolver for NtResolver {
    fn resolve_modules(
        &self,
        _stages: &[lex_ast::Stage],
        head_op: Option<&str>,
    ) -> BTreeMap<String, Ty> {
        let mut m = BTreeMap::new();
        // `None` means "resolve from the caller's working copy" — what the
        // client resolver does on the publish path, so it always resolves.
        // `Some(head)` is a *stored* head: a package resolves only if the lock
        // governing it (its own or, post-#975, an ancestor's) pins it.
        let pinned: Option<BTreeSet<String>> = match head_op {
            None => None,
            Some(head) => {
                let Ok(store) = Store::open(&self.root) else { return m };
                let Ok(Some(lock)) = store.committed_lock_inherited(head) else { return m };
                let Ok(lf) = lex_syntax::lock::LockFile::from_toml(&lock) else { return m };
                Some(lf.packages.into_iter().map(|e| e.name).collect())
            }
        };
        let int_fn = |arity: usize| {
            Ty::function(vec![Ty::int(); arity], EffectSet::empty(), Ty::int())
        };
        let catalog = [
            ("lex-nt", "lex-nt/lib", "gcd", 2),
            ("lex-mathx", "lex-mathx/lib", "sq", 1),
            ("lex-strx", "lex-strx/lib", "twice", 1),
        ];
        for (pkg, reference, fname, arity) in catalog {
            if pinned.as_ref().is_some_and(|p| !p.contains(pkg)) {
                continue;
            }
            let rec = module_record_from_fields(vec![(fname.to_string(), int_fn(arity))]);
            m.insert(reference.to_string(), rec);
        }
        m
    }
}

const LOCK: &str = "version = 1\n\n[[package]]\nname = \"lex-nt\"\nregistry = \"vcs.lexlang.org/lex-official/lex-nt\"\nconstraint = \"^1.0\"\nversion = \"1.0.0\"\nhead_op = \"op_nt\"\n";

const BASE: &str =
    "import \"lex-nt/lib\" as nt\nfn reduce(a :: Int, b :: Int) -> Int { nt.gcd(a, b) }\n";

fn named(src: &str, name: &str) -> lex_ast::Stage {
    canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == name))
        .expect("fn not found")
}

fn head_op_vec(s: &Store, branch: &str) -> Vec<String> {
    s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect()
}

/// A store whose `main` head is **non-inlined**: it imports `lex-nt/lib` as an
/// `AddImport` edge and calls `nt.gcd`, with the resolver installed.
fn store_with_non_inlined_head() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path())
        .unwrap()
        .with_dep_resolver(Arc::new(NtResolver { root: tmp.path().to_path_buf() }));

    let stages = canonicalize_program(&parse_source(BASE).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&BTreeMap::new(), &new, &et, &et, true);

    let mut imports: ImportMap = BTreeMap::new();
    let mut set = BTreeSet::new();
    set.insert(ImportRef { reference: "lex-nt/lib".to_string(), alias: "nt".to_string() });
    imports.insert("src/main.lex".to_string(), set);

    let head = store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &imports, true)
        .expect("publish (the client gate resolves via the resolver)")
        .head_op
        .expect("head op");
    // What `op push` does: commit the lock for the head it advances to. Only
    // *pushed* heads get one — which is the whole point of #975, since the
    // merge and patch heads created below inherit rather than carry their own.
    store.set_committed_lock(&head, LOCK).expect("commit lock");
    (store, tmp)
}

/// Land a fn on `branch` through the single-parent gate — i.e. the `/v1/patch`
/// write path, which builds its candidate from the head map.
fn land(s: &Store, branch: &str, src: &str, name: &str) {
    let st = named(src, name);
    let sig = lex_ast::sig_id(&st).unwrap();
    let stg = lex_ast::stage_id(&st).unwrap();
    s.publish(&st).unwrap();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stg.clone(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        },
        head_op_vec(s, branch),
    );
    let t = StageTransition::Create { sig_id: sig, stage_id: stg };
    s.apply_operation_gated(branch, op, t)
        .expect("the single-parent gate must resolve the head's dependencies");
}

#[test]
fn merge_projection_gate_resolves_a_non_inlined_head() {
    let (store, _tmp) = store_with_non_inlined_head();
    // An empty delta projects the head exactly as it stands, so this asserts the
    // narrowest possible thing: *merely type-checking an unchanged non-inlined
    // head* through the resolve-time gate must succeed.
    store
        .typecheck_merge_projection(DEFAULT_BRANCH, &BTreeMap::new())
        .expect("projecting an unchanged non-inlined head must type-check");
}

#[test]
fn patch_gate_resolves_a_non_inlined_head() {
    let (store, _tmp) = store_with_non_inlined_head();
    // A new fn that also calls through the dependency alias. `apply_operation_gated`
    // rebuilds the candidate from the head map (imports absent) and gates it.
    land(&store, DEFAULT_BRANCH, "fn twice(a :: Int) -> Int { nt.gcd(a, a) }\n", "twice");

    // A SECOND patch is the one that exercises #975: the first resolved against
    // the published head, which carries a pushed lock — but this one resolves
    // against the head the first patch just created, which has none of its own
    // and must inherit. A single patch would pass even with inheritance broken.
    land(&store, DEFAULT_BRANCH, "fn thrice(a :: Int) -> Int { nt.gcd(a, a) + a }\n", "thrice");

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.len(), 3, "reduce, twice and thrice should all be at the head: {head:?}");
}

#[test]
fn merge_commit_gate_resolves_a_non_inlined_head() {
    let (store, _tmp) = store_with_non_inlined_head();
    store.create_branch("feature", DEFAULT_BRANCH).unwrap();
    land(&store, "feature", "fn extra(a :: Int) -> Int { a + 1 }\n", "extra");

    let report = store.merge("feature", DEFAULT_BRANCH).expect("merge should compose");
    store
        .commit_merge(DEFAULT_BRANCH, &report)
        .expect("the merge gate must resolve the post-merge head's dependencies");

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert!(head.len() >= 2, "the merge should have landed extra alongside reduce: {head:?}");
}

// ---------------------------------------------------------------------------
// #977: a merge commits its OWN lock — the union of both parents' pins.
// ---------------------------------------------------------------------------

/// Render a `lex.lock` pinning `(name, version)` pairs.
fn lock_of(pins: &[(&str, &str)]) -> String {
    let mut s = String::from("version = 1\n");
    for (name, version) in pins {
        s.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nregistry = \"vcs.lexlang.org/lex-official/{name}\"\n\
             constraint = \"^{version}\"\nversion = \"{version}\"\nhead_op = \"op_{name}_{version}\"\n"
        ));
    }
    s
}

fn fns_of(src: &str) -> BTreeMap<String, lex_ast::FnDecl> {
    canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd)),
            _ => None,
        })
        .collect()
}

/// Publish the whole program `src` (previously `prev_src`) on `branch` with the
/// given `(reference, alias)` imports, then commit `lock` for the new head —
/// what `op push` does for a client-built head. Returns the new head.
fn push_program(
    store: &Store,
    branch: &str,
    prev_src: &str,
    src: &str,
    imports: &[(&str, &str)],
    lock: &str,
) -> String {
    let stages = canonicalize_program(&parse_source(src).expect("parse"));
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&fns_of(prev_src), &fns_of(src), &et, &et, true);
    let mut set = BTreeSet::new();
    for (reference, alias) in imports {
        set.insert(ImportRef { reference: reference.to_string(), alias: alias.to_string() });
    }
    let mut map: ImportMap = BTreeMap::new();
    map.insert("src/main.lex".to_string(), set);
    let head = store
        .publish_program(branch, &stages, &diff, &map, true)
        .expect("publish")
        .head_op
        .expect("head op");
    store.set_committed_lock(&head, lock).expect("commit lock");
    head
}

fn pinned_names(lock: &str) -> BTreeSet<String> {
    lex_syntax::lock::LockFile::from_toml(lock)
        .expect("a merged lock must parse as a lex.lock")
        .packages
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// The #977 case: the **feature** branch introduces a dependency the base lock
/// never mentioned, and main independently introduces a different one. The
/// merged head uses both, so it type-checks only against the *union* of the
/// two parents' pins.
///
/// Before the fix the merge op carried no lock of its own and inherited `dst`'s
/// whole (breadth-first, `dst` first), which does not pin `lex-mathx` — so the
/// merge gate rejected the head as `unknown_identifier "mx"`. Taking `src`'s
/// lock whole would fail the other way, on `sx`.
#[test]
fn merge_commits_the_union_of_both_parents_locks() {
    let (store, _tmp) = store_with_non_inlined_head();
    store.create_branch("feature", DEFAULT_BRANCH).unwrap();

    let feat_src = format!(
        "import \"lex-nt/lib\" as nt\nimport \"lex-mathx/lib\" as mx\n{}\
         fn sq_gcd(a :: Int, b :: Int) -> Int {{ mx.sq(nt.gcd(a, b)) }}\n",
        &BASE["import \"lex-nt/lib\" as nt\n".len()..]
    );
    let feat_head = push_program(
        &store,
        "feature",
        BASE,
        &feat_src,
        &[("lex-nt/lib", "nt"), ("lex-mathx/lib", "mx")],
        &lock_of(&[("lex-nt", "1.0.0"), ("lex-mathx", "0.3.0")]),
    );

    let main_src = format!(
        "import \"lex-nt/lib\" as nt\nimport \"lex-strx/lib\" as sx\n{}\
         fn dbl(a :: Int) -> Int {{ sx.twice(a) }}\n",
        &BASE["import \"lex-nt/lib\" as nt\n".len()..]
    );
    let main_head = push_program(
        &store,
        DEFAULT_BRANCH,
        BASE,
        &main_src,
        &[("lex-nt/lib", "nt"), ("lex-strx/lib", "sx")],
        &lock_of(&[("lex-nt", "1.0.0"), ("lex-strx", "2.1.0")]),
    );

    let report = store.merge("feature", DEFAULT_BRANCH).expect("merge should compose");
    assert!(report.conflicts.is_empty(), "disjoint edits must not conflict: {:?}", report.conflicts);
    store
        .commit_merge(DEFAULT_BRANCH, &report)
        .expect("the merged head must resolve BOTH branches' dependencies");

    let merge_head = store
        .get_branch(DEFAULT_BRANCH)
        .unwrap()
        .and_then(|b| b.head_op)
        .expect("main has a head");
    assert_ne!(merge_head, main_head, "a merge op must have landed");
    assert_ne!(merge_head, feat_head);

    // The merge head carries its OWN lock (no inheritance needed) and it is
    // the union of both parents' pins.
    let own = store
        .committed_lock(&merge_head)
        .unwrap()
        .expect("the merge op must commit its own lock (#977)");
    let expected: BTreeSet<String> =
        ["lex-mathx", "lex-nt", "lex-strx"].iter().map(|s| s.to_string()).collect();
    assert_eq!(pinned_names(&own), expected, "merged lock must be the union: {own}");

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.len(), 3, "reduce, sq_gcd and dbl should all be at the merged head: {head:?}");
}

/// Both parents pin the **same** package at **different** versions. Silently
/// picking one would change what the merged code was tested against, so the
/// merge is refused with a dependency-conflict error naming the package and
/// both versions — and, per the always-valid-HEAD invariant, neither branch
/// moves.
#[test]
fn merge_refuses_a_same_package_different_version_conflict() {
    let (store, _tmp) = store_with_non_inlined_head();
    store.create_branch("feature", DEFAULT_BRANCH).unwrap();

    // feature bumps lex-nt to 2.0.0 (a pushed head with its own lock) …
    land(&store, "feature", "fn extra(a :: Int) -> Int { a + 1 }\n", "extra");
    let feat_head = head_op_vec(&store, "feature").pop().unwrap();
    store
        .set_committed_lock(&feat_head, &lock_of(&[("lex-nt", "2.0.0")]))
        .unwrap();

    // … while main keeps 1.0.0 and moves on independently.
    land(&store, DEFAULT_BRANCH, "fn other(a :: Int) -> Int { a + 2 }\n", "other");
    let main_before = head_op_vec(&store, DEFAULT_BRANCH).pop().unwrap();

    let report = store.merge("feature", DEFAULT_BRANCH).expect("merge report");
    assert!(report.conflicts.is_empty(), "the code itself composes: {:?}", report.conflicts);
    let err = store
        .commit_merge(DEFAULT_BRANCH, &report)
        .expect_err("a lex-nt 1.0.0 vs 2.0.0 conflict must refuse the merge");

    match &err {
        lex_store::StoreError::DependencyConflict { package, dst_version, src_version } => {
            assert_eq!(package, "lex-nt");
            assert_eq!(dst_version, "1.0.0");
            assert_eq!(src_version, "2.0.0");
        }
        other => panic!("expected DependencyConflict, got {other:?}"),
    }
    let msg = err.to_string();
    for needle in ["lex-nt", "1.0.0", "2.0.0"] {
        assert!(msg.contains(needle), "error must name {needle}: {msg}");
    }

    // Always-valid HEAD: nothing advanced.
    assert_eq!(head_op_vec(&store, DEFAULT_BRANCH), vec![main_before], "main must not move");
    assert_eq!(head_op_vec(&store, "feature"), vec![feat_head], "feature must not move");
    assert!(
        store.get_branch(DEFAULT_BRANCH).unwrap().unwrap().merges.is_empty(),
        "a refused merge must not be journaled"
    );
}
