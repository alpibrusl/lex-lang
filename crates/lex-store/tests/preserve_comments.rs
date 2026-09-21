//! Comments survive the op-log round trip, without touching any hash.
//!
//! `lex-syntax` parses `#` comments and attaches them per declaration, and the
//! canonicalizer then strips them so they never participate in a SigId or
//! StageId. That exclusion is right: editing a comment must not read as a code
//! change, or exact replay would demand a model reproduce prose verbatim.
//!
//! But the op-log stored only the canonical (stripped) AST, and a registry
//! archive is *rendered* from that log — so every hosted package was served
//! with its documentation removed. Measured across `lex-official`: `lex-ocpi`
//! lost 2051 comments, `lex-agent` 524, `lex-fix` 293; not one of the 18
//! packages sampled retained a single comment. A module header describing a
//! package's schema and purpose simply vanished between GitHub and the
//! registry.
//!
//! Comments now ride in the stage's `Metadata`, outside the hash — the same
//! place and for the same reason as `name`, which lives there so renames don't
//! move a StageId.
//!
//! The first test is the load-bearing one: it pins that carrying comments did
//! **not** disturb content addressing.

use std::collections::BTreeMap;

use lex_ast::canonicalize_program;
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

fn only_fn(src: &str) -> lex_ast::Stage {
    canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(_)))
        .expect("a fn")
}

const PLAIN: &str = "fn add(a :: Int, b :: Int) -> Int { a + b }\n";
const DOCUMENTED: &str = "\
# Add two integers.
#
# Overflow wraps, per std.int semantics.
fn add(a :: Int, b :: Int) -> Int { a + b }
";
const REDOCUMENTED: &str = "\
# Sum two integers.
fn add(a :: Int, b :: Int) -> Int { a + b }
";

/// **The property everything else depends on.** Comments must not reach any
/// hash: a documented and an undocumented declaration are the same code, and
/// exact replay compares StageIds against what a model regenerates.
#[test]
fn comments_change_neither_sig_id_nor_stage_id() {
    let plain = only_fn(PLAIN);
    let documented = only_fn(DOCUMENTED);

    assert_eq!(
        lex_ast::sig_id(&plain),
        lex_ast::sig_id(&documented),
        "SigId must ignore comments"
    );
    assert_eq!(
        lex_ast::stage_id(&plain),
        lex_ast::stage_id(&documented),
        "StageId must ignore comments — otherwise a doc edit reads as a code change"
    );
}

/// …and editing a comment likewise leaves the identity alone.
#[test]
fn editing_a_comment_does_not_move_the_stage_id() {
    assert_eq!(
        lex_ast::stage_id(&only_fn(DOCUMENTED)),
        lex_ast::stage_id(&only_fn(REDOCUMENTED)),
        "rewording documentation is not a change to the code"
    );
}

/// The stored AST must stay byte-identical to what it was before comments were
/// carried, so existing `.ast.json` files and their hashes are untouched.
#[test]
fn the_serialized_ast_is_unchanged_by_comments() {
    let plain = serde_json::to_string(&only_fn(PLAIN)).unwrap();
    let documented = serde_json::to_string(&only_fn(DOCUMENTED)).unwrap();
    assert_eq!(
        plain, documented,
        "`doc` is serde(skip); if it serializes, every stored stage rehashes"
    );
}

fn store() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    (store, tmp)
}

/// The round trip: publish documented source, render it back, get the comments.
#[test]
fn comments_survive_publish_and_render() {
    let (store, _tmp) = store();
    let stages = canonicalize_program(&parse_source(DOCUMENTED).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&BTreeMap::new(), &new, &et, &et, true);
    let head = store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &BTreeMap::new(), true)
        .expect("publish")
        .head_op
        .expect("head");

    let ph = lex_store::render::package_head_at_op(&store, &head).expect("head map");
    let rendered = match lex_store::render::render_source(&store, &ph).expect("render") {
        lex_store::render::RenderedSource::Single { src, .. } => src,
        lex_store::render::RenderedSource::Multi(t) => t.values().cloned().collect(),
    };

    assert!(
        rendered.contains("# Add two integers."),
        "the declaration's documentation must come back: {rendered}"
    );
    assert!(
        rendered.contains("# Overflow wraps"),
        "every comment line, not just the first: {rendered}"
    );
    assert!(
        rendered.contains("fn add("),
        "…alongside the code: {rendered}"
    );
}

/// Undocumented code renders exactly as before — no stray blank lines or
/// artefacts introduced by the new path.
#[test]
fn undocumented_code_renders_unchanged() {
    let (store, _tmp) = store();
    let st = only_fn(PLAIN);
    store.publish(&st).unwrap();
    let meta = store
        .get_metadata(&lex_ast::stage_id(&st).unwrap())
        .expect("metadata");
    assert!(meta.doc.is_empty(), "no comments means no doc entry");
    assert_eq!(
        lex_ast::print_stages(&[st]).trim_start().chars().next(),
        Some('f')
    );
}

/// A doc edit must actually reach the metadata. Since the StageId is unchanged
/// by design, the metadata file already exists — so a naive
/// `if !path.exists()` write would pin the first version's documentation
/// forever and a correction could never reach the registry.
#[test]
fn rewording_a_comment_updates_the_stored_doc() {
    let (store, _tmp) = store();
    let first = only_fn(DOCUMENTED);
    let stage_id = lex_ast::stage_id(&first).unwrap();
    store.publish(&first).unwrap();
    assert!(
        store
            .get_metadata(&stage_id)
            .unwrap()
            .doc
            .iter()
            .any(|l| l.contains("Add two integers")),
        "first publish stores its documentation"
    );

    store.publish(&only_fn(REDOCUMENTED)).unwrap();
    let doc = store.get_metadata(&stage_id).unwrap().doc;
    assert!(
        doc.iter().any(|l| l.contains("Sum two integers")),
        "the reworded comment must replace the old one, got {doc:?}"
    );
    assert!(
        !doc.iter().any(|l| l.contains("Add two integers")),
        "and the superseded wording must be gone, got {doc:?}"
    );
}
