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

/// Supplies `lex-nt/lib` with `gcd(Int, Int) -> Int` — but **only for a head
/// whose pins it can actually find**, exactly like the real `HubDepResolver`,
/// which reads the committed lock.
///
/// This lock-awareness is deliberate. A resolver that answers unconditionally
/// hides #975: it would resolve for a merge or `/v1/patch` head that carries no
/// lock, so the gate tests would pass while production failed with
/// `unknown_identifier`. That is precisely what happened — the original mock
/// here was unconditional, and only a live prod merge exposed the gap. Keeping
/// the fixture honest about the lock makes that class of bug catchable here.
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
        // `Some(head)` is a *stored* head: it resolves only if a lock governs
        // it (its own or, post-#975, an ancestor's), mirroring the hub.
        if let Some(head) = head_op {
            let Ok(store) = Store::open(&self.root) else { return m };
            if !matches!(store.committed_lock_inherited(head), Ok(Some(_))) {
                return m;
            }
        }
        let rec = module_record_from_fields(vec![(
            "gcd".to_string(),
            Ty::function(vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int()),
        )]);
        m.insert("lex-nt/lib".to_string(), rec);
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
