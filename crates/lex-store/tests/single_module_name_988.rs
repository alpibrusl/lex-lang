//! #988: a single-module head must render under **its own** file name.
//!
//! `RenderedSource::Single` used to carry only the source text, so both
//! callers — the registry archive endpoint and `lex export-git` — invented
//! `src/lib.lex` for it. That silently *renamed* any package whose module was
//! not called `lib`. `lex-jobs` ships `src/jobs.lex`, so what the registry
//! served was:
//!
//! ```text
//! lex.toml
//! src/lib.lex
//! ```
//!
//! and every dependent writing `import "lex-jobs/src/jobs"` failed to resolve
//! a module that was right there under another name. Three libraries in the
//! migration dry-run (`lex-loom`, `lex-oms`, `lex-soft`) are blocked by
//! exactly this, across 7 import sites.
//!
//! **The case that matters is the mixed one.** A head renders multi-module
//! only when *every* sig records an `in_file`; a package published before
//! `in_file` existed records none, and the hosted `lex-jobs`, `lex-nt`,
//! `lex-semver` and `lex-modmath` heads are all in that state (0 of 20, 0 of
//! 19, 0 of 8 …). Re-pushing such a package onto its existing op-log leaves
//! the head *partly* attributed — new ops carry `in_file`, surviving old ones
//! do not — so it still renders as `Single` while knowing perfectly well which
//! file it came from. That is the shape these tests pin: a head with partial
//! attribution must use the name it has rather than fall back to `lib`.

use std::collections::BTreeSet;

use lex_ast::canonicalize_program;
use lex_store::render::{render_source, PackageHead, RenderedSource};
use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

fn named(src: &str, name: &str) -> lex_ast::Stage {
    canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == name))
        .expect("fn present")
}

fn head_op_vec(s: &Store, branch: &str) -> Vec<String> {
    s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect()
}

/// Land one declaration on `main`, attributed to `in_file` (or to nothing,
/// like an op from before `in_file` was recorded).
fn land(store: &Store, src: &str, name: &str, in_file: Option<&str>) {
    let st = named(src, name);
    let sig = lex_ast::sig_id(&st).unwrap();
    let stage = lex_ast::stage_id(&st).unwrap();
    store.publish(&st).unwrap();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stage.clone(),
            effects: BTreeSet::new(),
            budget_cost: None,
            in_file: in_file.map(str::to_string),
        },
        head_op_vec(store, DEFAULT_BRANCH),
    );
    store
        .apply_operation(
            DEFAULT_BRANCH,
            op,
            StageTransition::Create { sig_id: sig, stage_id: stage },
        )
        .expect("apply");
}

fn head_of(store: &Store) -> PackageHead {
    let head = store.get_branch(DEFAULT_BRANCH).unwrap().and_then(|b| b.head_op).expect("head");
    lex_store::render::package_head_at_op(store, &head).expect("package head")
}

fn store() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    (store, tmp)
}

const ENQUEUE: &str = "fn enqueue(n :: Int) -> Int { n + 1 }\n";
const DRAIN: &str = "fn drain(n :: Int) -> Int { n - 1 }\n";

/// The re-push shape: one op knows its file, an older one does not. The head
/// is still `Single` — but it is not nameless, so `lib` would be a lie.
#[test]
fn a_partly_attributed_head_renders_under_the_name_it_knows() {
    let (store, _tmp) = store();
    land(&store, ENQUEUE, "enqueue", Some("src/jobs.lex"));
    land(&store, DRAIN, "drain", None);

    let head = head_of(&store);
    assert_eq!(head.sig_files.len(), 1, "exactly one sig is attributed");
    assert_eq!(head.map.len(), 2, "…out of two at the head, so this renders Single");

    match render_source(&store, &head).expect("render") {
        RenderedSource::Single { path, src } => {
            assert_eq!(path.as_deref(), Some("src/jobs.lex"), "the head's own file, not an invented one");
            assert!(src.contains("fn enqueue(") && src.contains("fn drain("),
                "both declarations belong to the one module: {src}");
        }
        RenderedSource::Multi(tree) => panic!("expected a single module, got {:?}", tree.keys()),
    }
}

/// A head that records no file at all cannot name itself — and must not
/// pretend to. This is every package published before `in_file` existed, so
/// the fallback is load-bearing, not decoration.
#[test]
fn an_unattributed_head_falls_back_rather_than_guessing() {
    let (store, _tmp) = store();
    land(&store, ENQUEUE, "enqueue", None);

    let head = head_of(&store);
    assert!(head.sig_files.is_empty(), "nothing attributed");
    match render_source(&store, &head).expect("render") {
        RenderedSource::Single { path, .. } => assert_eq!(path, None, "a nameless head must say so, not guess"),
        RenderedSource::Multi(tree) => panic!("expected a single module, got {:?}", tree.keys()),
    }
}

/// A fully attributed head is multi-module and keeps every real name — the
/// arm that was always correct, asserted so the two cannot drift apart.
#[test]
fn a_fully_attributed_head_keeps_every_real_name() {
    let (store, _tmp) = store();
    land(&store, ENQUEUE, "enqueue", Some("src/jobs.lex"));
    land(&store, DRAIN, "drain", Some("src/drain.lex"));

    match render_source(&store, &head_of(&store)).expect("render") {
        RenderedSource::Multi(tree) => {
            let files: Vec<&String> = tree.keys().collect();
            assert_eq!(files, vec!["src/drain.lex", "src/jobs.lex"], "got {files:?}");
        }
        RenderedSource::Single { path, .. } => {
            panic!("a fully attributed head must render multi-module, got {path:?}")
        }
    }
}

/// A single-module head whose one file *is* `lib` is unchanged — it was
/// accidentally correct before, and must stay correct for the new reason.
#[test]
fn a_module_actually_called_lib_is_unaffected() {
    let (store, _tmp) = store();
    land(&store, ENQUEUE, "enqueue", Some("src/lib.lex"));
    land(&store, DRAIN, "drain", None);

    match render_source(&store, &head_of(&store)).expect("render") {
        RenderedSource::Single { path, .. } => assert_eq!(path.as_deref(), Some("src/lib.lex")),
        RenderedSource::Multi(tree) => panic!("expected a single module, got {:?}", tree.keys()),
    }
}

/// Defensive: several distinct files but a `Single` render (a head where some
/// sigs lost their attribution) has no honest name to pick, so it must fall
/// back rather than choose one of them arbitrarily.
#[test]
fn a_single_render_spanning_several_named_files_does_not_pick_one() {
    let (store, _tmp) = store();
    land(&store, ENQUEUE, "enqueue", Some("src/jobs.lex"));
    land(&store, DRAIN, "drain", Some("src/drain.lex"));
    land(&store, "fn idle(n :: Int) -> Int { n }\n", "idle", None);

    let head = head_of(&store);
    assert_eq!(head.sig_files.len(), 2, "two files attributed, three sigs at the head");
    match render_source(&store, &head).expect("render") {
        RenderedSource::Single { path, .. } => assert_eq!(
            path, None,
            "two candidate names is not a name; it must not pick one"
        ),
        RenderedSource::Multi(tree) => panic!("expected a single module, got {:?}", tree.keys()),
    }
}

/// A `BTreeMap` iterates in sorted order, so a bug that silently took "the
/// first file" would be invisible in the test above whenever the right answer
/// sorts first. Reversing the landing order pins that it is the *count* that
/// decides, not the ordering.
#[test]
fn the_fallback_is_not_hiding_a_first_wins_rule() {
    let (store, _tmp) = store();
    land(&store, ENQUEUE, "enqueue", Some("src/aaa.lex"));
    land(&store, DRAIN, "drain", Some("src/zzz.lex"));
    land(&store, "fn idle(n :: Int) -> Int { n }\n", "idle", None);

    match render_source(&store, &head_of(&store)).expect("render") {
        RenderedSource::Single { path, .. } => {
            assert_ne!(
                path.as_deref(), Some("src/aaa.lex"),
                "must not take the alphabetically first file"
            );
            assert_eq!(path, None);
        }
        RenderedSource::Multi(tree) => panic!("expected a single module, got {:?}", tree.keys()),
    }
}
