//! #992: changing a function's effects must move its head entry to the new
//! SigId, not leave the old one pointing at the new stage.
//!
//! A SigId covers the effect row — that is why the op is called
//! *ChangeEffectSig*. But its transition was a `Replace`, and
//! `apply_transition` implements `Replace` as `map.insert(sig_id, to)`: the
//! **old** sig, bound to the **new** stage. A store files an implementation
//! under the sig its own AST hashes to, and that AST declares the new effects,
//! so the head ended up holding a pair no store can ever satisfy.
//!
//! The consequence was not theoretical. `lex-official/lex-web`'s head carries
//!
//! ```text
//! add_function       sig b5898dc6  stage 4b80ad35        <- AST filed here
//! change_effect_sig  sig 992acb83  c9b52540 -> 4b80ad35  <- old sig, new stage
//! ```
//!
//! for one declaration (`serve_from_dir`, `[io]` -> `[fs_read]`), so the head
//! names it twice and one entry is unrenderable. Every archive render of that
//! head returns `500 unknown stage_id`, and `lex-web@0.4.0` — cut from it — is
//! permanently broken, releases being immutable.
//!
//! These tests pin the head *map*, because that is what rendering reads and
//! what a release is cut from. Asserting the op's fields alone would pass on a
//! transition that still bound the wrong sig.

use std::collections::{BTreeMap, BTreeSet};

use lex_ast::canonicalize_program;
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

/// Publish `src` as the whole program and return the resulting head map.
fn publish(store: &Store, src: &str) -> BTreeMap<String, String> {
    let stages = canonicalize_program(&parse_source(src).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let old: BTreeMap<String, lex_ast::FnDecl> = prior_fns(store);
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&old, &new, &et, &et, true);
    store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &BTreeMap::new(), true)
        .expect("publish");
    store.branch_head(DEFAULT_BRANCH).expect("head")
}

/// The functions currently at the head, by name — the baseline a republish
/// diffs against.
fn prior_fns(store: &Store) -> BTreeMap<String, lex_ast::FnDecl> {
    let head = match store.branch_head(DEFAULT_BRANCH) {
        Ok(h) => h,
        Err(_) => return BTreeMap::new(),
    };
    let pairs: Vec<(String, String)> = head.into_iter().collect();
    let mut out = BTreeMap::new();
    for ((_, _), ast) in pairs.iter().zip(store.get_asts_for_sigs_bulk(&pairs)) {
        if let Ok(lex_ast::Stage::FnDecl(fd)) = ast {
            out.insert(fd.name.clone(), fd);
        }
    }
    out
}

fn store() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    (store, tmp)
}

/// `serve` reads a file; the effect row is what changes between the two.
const WITH_IO: &str = "fn serve(p :: Str) -> [io] Str { p }\n";
const WITH_FS: &str = "fn serve(p :: Str) -> [fs_read] Str { p }\n";

fn sig_of(src: &str) -> String {
    let stages = canonicalize_program(&parse_source(src).expect("parse"));
    let st = stages
        .iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(_)))
        .expect("a fn");
    lex_ast::sig_id(st).expect("sig")
}

/// The premise: an effect change really does change the SigId. If this ever
/// stopped being true the rest of this file would be vacuous, so assert it
/// rather than assume it.
#[test]
fn changing_effects_changes_the_sig_id() {
    assert_ne!(
        sig_of(WITH_IO),
        sig_of(WITH_FS),
        "a SigId must cover the effect row — otherwise ChangeEffectSig is misnamed"
    );
}

/// The bug itself. After the effect change the head must name the **new** sig
/// and no longer the old one.
#[test]
fn an_effect_change_moves_the_head_entry_to_the_new_sig() {
    let (store, _tmp) = store();
    let old_sig = sig_of(WITH_IO);
    let new_sig = sig_of(WITH_FS);

    let head = publish(&store, WITH_IO);
    assert_eq!(head.keys().collect::<Vec<_>>(), vec![&old_sig], "baseline: {head:?}");

    let head = publish(&store, WITH_FS);
    assert!(
        head.contains_key(&new_sig),
        "the head must bind the sig the new AST hashes to: {head:?}"
    );
    assert!(
        !head.contains_key(&old_sig),
        "and must not keep the old sig — that entry is unsatisfiable: {head:?}"
    );
    assert_eq!(head.len(), 1, "one declaration means one head entry: {head:?}");
}

/// The property that actually matters: every pair the head names must be
/// readable back. This is the check the archive renderer performs, and the one
/// `lex-web`'s head fails.
#[test]
fn every_head_pair_resolves_to_an_ast_after_an_effect_change() {
    let (store, _tmp) = store();
    publish(&store, WITH_IO);
    let head = publish(&store, WITH_FS);

    let pairs: Vec<(String, String)> = head.iter().map(|(s, t)| (s.clone(), t.clone())).collect();
    for ((sig, stage), ast) in pairs.iter().zip(store.get_asts_for_sigs_bulk(&pairs)) {
        assert!(
            ast.is_ok(),
            "head names ({sig}, {stage}) but no AST is filed under that sig — \
             exactly the shape that makes a release unrenderable"
        );
    }
}

/// …and the declaration that comes back is the new one, not a stale body kept
/// alive under the old identity.
#[test]
fn the_resolved_declaration_carries_the_new_effects() {
    let (store, _tmp) = store();
    publish(&store, WITH_IO);
    let head = publish(&store, WITH_FS);

    let pairs: Vec<(String, String)> = head.iter().map(|(s, t)| (s.clone(), t.clone())).collect();
    let asts = store.get_asts_for_sigs_bulk(&pairs);
    let fd = asts
        .into_iter()
        .find_map(|a| match a {
            Ok(lex_ast::Stage::FnDecl(fd)) => Some(fd),
            _ => None,
        })
        .expect("one fn at the head");
    let effects: BTreeSet<String> = fd.effects.iter().map(|e| e.name.clone()).collect();
    assert_eq!(effects, BTreeSet::from(["fs_read".to_string()]), "got {effects:?}");
}

/// A body-only change still takes the plain `Replace` path: the sig is
/// unchanged, so there is nothing to move and the old behaviour is correct.
#[test]
fn a_body_only_change_keeps_the_same_sig() {
    let (store, _tmp) = store();
    let before = publish(&store, "fn f(x :: Int) -> Int { x + 1 }\n");
    let after = publish(&store, "fn f(x :: Int) -> Int { x + 2 }\n");

    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "a body change must not move the declaration to another sig"
    );
    assert_ne!(
        before.values().collect::<Vec<_>>(),
        after.values().collect::<Vec<_>>(),
        "…but the stage must have changed"
    );
}
