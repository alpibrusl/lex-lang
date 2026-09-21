//! #992, repair half: a publish retires a head entry that no store can hold.
//!
//! Fixing `ChangeEffectSig` stops new damage but cannot undo what is already
//! on disk. The publish diff is keyed by declaration name and reads the old
//! side through the ASTs it *can* resolve, so a stranded entry is invisible to
//! it — no op is emitted, and the head stays unrenderable however many times
//! the package is republished. Verified against the real `lex-web` store: a
//! republish there emits zero ops.
//!
//! The damage looks like this, from `lex-official/lex-web`'s actual head:
//!
//! ```text
//! add_function       sig b5898dc6  stage 4b80ad35        <- AST filed here
//! change_effect_sig  sig 992acb83  c9b52540 -> 4b80ad35  <- old sig, new stage
//! ```
//!
//! Both are `serve_from_dir` ([io] -> [fs_read]). The head names it twice and
//! `(992acb83, 4b80ad35)` can never resolve, because the AST at that stage
//! declares the new effects and so hashes to `b5898dc6`.
//!
//! Since the bug is fixed, these tests cannot *produce* that shape by
//! publishing — they construct it directly, which is the only way to test a
//! repair for damage that can no longer be created.

use std::collections::{BTreeMap, BTreeSet};

use lex_ast::canonicalize_program;
use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

const WITH_IO: &str = "fn serve(p :: Str) -> [io] Str { p }\n";
const WITH_FS: &str = "fn serve(p :: Str) -> [fs_read] Str { p }\n";
const OTHER: &str = "fn untouched(n :: Int) -> Int { n + 1 }\n";

fn only_fn(src: &str) -> lex_ast::Stage {
    canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(_)))
        .expect("a fn")
}

fn ids(src: &str) -> (String, String) {
    let st = only_fn(src);
    (lex_ast::sig_id(&st).expect("sig"), lex_ast::stage_id(&st).expect("stage"))
}

fn head_op_vec(s: &Store) -> Vec<String> {
    s.get_branch(DEFAULT_BRANCH).unwrap().and_then(|b| b.head_op).into_iter().collect()
}

/// Land a declaration under its own (correct) sig.
fn land(store: &Store, src: &str) {
    let st = only_fn(src);
    let (sig, stage) = ids(src);
    store.publish(&st).unwrap();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stage.clone(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: None,
        },
        head_op_vec(store),
    );
    store
        .apply_operation(DEFAULT_BRANCH, op, StageTransition::Create { sig_id: sig, stage_id: stage })
        .expect("apply");
}

/// Bind `sig` to `stage` even though `stage`'s AST is filed under another sig
/// — the pre-#992 `Replace`, reproduced directly.
fn strand(store: &Store, sig: &str, from: &str, to: &str) {
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.to_string(),
            from_stage_id: from.to_string(),
            to_stage_id: to.to_string(),
            from_budget: None,
            to_budget: None,
        },
        head_op_vec(store),
    );
    store
        .apply_operation(
            DEFAULT_BRANCH,
            op,
            StageTransition::Replace {
                sig_id: sig.to_string(),
                from: from.to_string(),
                to: to.to_string(),
            },
        )
        .expect("apply");
}

/// A store whose head carries exactly the `lex-web` damage: one declaration
/// under two sigs, the old one pointing at a stage filed under the new one.
fn damaged() -> (Store, tempfile::TempDir, String, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (sig_io, stage_io) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);

    land(&store, WITH_IO);
    land(&store, WITH_FS);
    land(&store, OTHER);
    strand(&store, &sig_io, &stage_io, &stage_fs);

    (store, tmp, sig_io, sig_fs, stage_fs)
}

fn publish_source(store: &Store, srcs: &[&str]) {
    let program: String = srcs.concat();
    let stages = canonicalize_program(&parse_source(&program).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let head = store.branch_head(DEFAULT_BRANCH).expect("head");
    let pairs: Vec<(String, String)> = head.into_iter().collect();
    let old: BTreeMap<String, lex_ast::FnDecl> = pairs
        .iter()
        .zip(store.get_asts_for_sigs_bulk(&pairs))
        .filter_map(|(_, ast)| match ast.ok()? {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd)),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&old, &new, &et, &et, true);
    store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &BTreeMap::new(), true)
        .expect("publish");
}

fn unresolvable(store: &Store) -> Vec<(String, String)> {
    let head = store.branch_head(DEFAULT_BRANCH).expect("head");
    let pairs: Vec<(String, String)> = head.into_iter().collect();
    pairs
        .iter()
        .zip(store.get_asts_for_sigs_bulk(&pairs))
        .filter(|(_, ast)| ast.is_err())
        .map(|((s, t), _)| (s.clone(), t.clone()))
        .collect()
}

/// The fixture must really be damaged, or everything below is vacuous.
#[test]
fn the_fixture_reproduces_the_prod_damage() {
    let (store, _tmp, sig_io, sig_fs, stage_fs) = damaged();
    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get(&sig_io), Some(&stage_fs), "old sig points at the new stage");
    assert_eq!(head.get(&sig_fs), Some(&stage_fs), "…as does the sig that owns it");
    assert_eq!(
        unresolvable(&store),
        vec![(sig_io, stage_fs)],
        "exactly one head pair must be unreadable"
    );
}

/// The repair: republishing the package retires the stranded entry.
#[test]
fn a_publish_retires_the_stranded_entry() {
    let (store, _tmp, sig_io, sig_fs, _) = damaged();
    publish_source(&store, &[WITH_FS, OTHER]);

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert!(!head.contains_key(&sig_io), "the unreadable entry must be gone: {head:?}");
    assert!(head.contains_key(&sig_fs), "the readable one must remain: {head:?}");
    assert!(
        unresolvable(&store).is_empty(),
        "every head pair must resolve after the repair: {:?}",
        unresolvable(&store)
    );
}

/// …and the repair does not take the rest of the package with it.
#[test]
fn unrelated_declarations_survive_the_repair() {
    let (store, _tmp, _, _, _) = damaged();
    let (sig_other, _) = ids(OTHER);
    publish_source(&store, &[WITH_FS, OTHER]);

    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    assert!(head.contains_key(&sig_other), "an untouched declaration must remain: {head:?}");
}

/// The guard. A store that is merely *missing* a blob — mid-pull, a partial
/// clone, a GC'd object — must be left strictly alone: nothing proves the
/// content exists elsewhere, so dropping the entry would destroy history.
#[test]
fn a_merely_absent_blob_is_never_retired() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    land(&store, WITH_IO);
    land(&store, OTHER);

    // Bind a sig to a stage nothing has ever stored, under any sig.
    let (sig_io, stage_io) = ids(WITH_IO);
    strand(&store, &sig_io, &stage_io, &"f".repeat(64));

    let before = unresolvable(&store);
    assert_eq!(before.len(), 1, "setup: one unreadable pair");

    let heal = store.stranded_head_entries(
        &store.branch_head(DEFAULT_BRANCH).unwrap().into_iter().collect::<Vec<_>>(),
    );
    assert!(
        heal.is_empty(),
        "an absent blob is not proof of damage — retiring it would lose history: {heal:?}"
    );
}

/// A healthy head is untouched, so the check costs nothing in normal use and
/// cannot silently erode a package over repeated publishes.
#[test]
fn a_healthy_head_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    land(&store, WITH_FS);
    land(&store, OTHER);

    let pairs: Vec<(String, String)> =
        store.branch_head(DEFAULT_BRANCH).unwrap().into_iter().collect();
    assert!(store.stranded_head_entries(&pairs).is_empty());

    let before = store.branch_head(DEFAULT_BRANCH).unwrap();
    publish_source(&store, &[WITH_FS, OTHER]);
    assert_eq!(before, store.branch_head(DEFAULT_BRANCH).unwrap(), "no-op republish must not move the head");
}
