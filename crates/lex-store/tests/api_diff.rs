//! #893 version-bump gate — public-API extraction + change classification on
//! a real op-log built through `publish_program`, the same heads the release
//! handler diffs.

use std::collections::BTreeMap;

use lex_store::api::{classify_api_change, public_api_at_op, ApiChange};
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;

fn fns(src: &str) -> BTreeMap<String, lex_ast::FnDecl> {
    lex_ast::canonicalize_program(&parse_source(src).expect("parse"))
        .into_iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd)),
            _ => None,
        })
        .collect()
}

/// Publish `src` as the next head (diffed against `old`), returning the new
/// head op and the fn-set for the next diff.
fn publish(
    s: &Store,
    old: &BTreeMap<String, lex_ast::FnDecl>,
    src: &str,
) -> (String, BTreeMap<String, lex_ast::FnDecl>) {
    let stages = lex_ast::canonicalize_program(&parse_source(src).expect("parse"));
    let new = fns(src);
    let et: BTreeMap<String, lex_ast::TypeDecl> = Default::default();
    let diff = lex_vcs::compute_diff_with_types(old, &new, &et, &et, true);
    let imports = lex_vcs::ImportMap::new();
    let head = s
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &imports, true)
        .unwrap()
        .head_op
        .expect("head_op");
    (head, new)
}

#[test]
fn api_diff_classifies_breaking_additive_and_none() {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();

    // v1: one function.
    let (h1, f1) = publish(&s, &Default::default(), "fn f(x :: Int) -> Int { x }\n");
    // v2: add a second function → additive.
    let (h2, f2) = publish(&s, &f1, "fn f(x :: Int) -> Int { x }\nfn g() -> Int { 7 }\n");
    // v3: change f's body only → no API change.
    let (h3, _f3) = publish(&s, &f2, "fn f(x :: Int) -> Int { x + 0 }\nfn g() -> Int { 7 }\n");

    let a1 = public_api_at_op(&s, &h1).unwrap();
    let a2 = public_api_at_op(&s, &h2).unwrap();
    let a3 = public_api_at_op(&s, &h3).unwrap();

    // v1 has exactly one public fn; v2 adds one.
    assert_eq!(a1.len(), 1, "v1 api: {a1:?}");
    assert_eq!(a2.len(), 2, "v2 api: {a2:?}");

    assert!(matches!(classify_api_change(&a1, &a2), ApiChange::Additive(_)), "v1→v2 additive");
    assert_eq!(classify_api_change(&a2, &a3), ApiChange::None, "v2→v3 body-only = none");
    // A removal is breaking (comparing the v2 API against the v1 API: g dropped).
    assert!(matches!(classify_api_change(&a2, &a1), ApiChange::Breaking(_)), "removal = breaking");
}
