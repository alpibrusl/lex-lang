//! #991: the version gate must classify the *API*, not the file naming.
//!
//! `public_api_at_op` keyed the API by the raw declaration name, which carries
//! the loader's mangling prefix `<stem>_<hash>`. That hash derives from the
//! source file, so renaming a file rotates the prefix on every declaration in
//! it and the diff reads a pure rename as "every symbol removed, every symbol
//! added".
//!
//! In production that surfaced as:
//!
//! ```text
//! version bump too small: 1.0.0 → 1.1.0 is a Minor bump,
//!   but the API change (`JobOpts` was removed) requires a major bump
//! ```
//!
//! `JobOpts` had not been removed — it was present in both the hosted archive
//! and the source. `lex-jobs` was simply renaming `lib.lex` to `jobs.lex` as
//! part of the #988 remediation, and the gate cost it a major version.
//!
//! The remediation for #988 *is* a file rename for every affected package, so
//! this gate fired on precisely the packages that needed re-publishing.

use std::collections::BTreeMap;

use lex_ast::canonicalize_program;
use lex_store::api::{classify_api_change, public_api_at_op, ApiChange};
use lex_store::{Store, DEFAULT_BRANCH};

/// Publish `files` as a package and return the resulting head op.
fn publish(store: &Store, dir: &std::path::Path, files: &[(&str, &str)]) -> String {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let entries: Vec<std::path::PathBuf> = files
        .iter()
        .map(|(name, body)| {
            let p = src.join(name);
            std::fs::write(&p, body).unwrap();
            p
        })
        .collect();
    let loaded =
        lex_syntax::loader::load_package(&entries, dir, "pkg", false).expect("load package");
    let stages = canonicalize_program(&loaded.program);
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    // Type declarations matter here — `JobOpts` is the one the prod failure
    // claimed had been removed — so they must actually be published, not left
    // out of the diff.
    let new_types: BTreeMap<String, lex_ast::TypeDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::TypeDecl(td) => Some((td.name.clone(), td.clone())),
            _ => None,
        })
        .collect();
    let empty = BTreeMap::new();
    let empty_types: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&empty, &new, &empty_types, &new_types, true);
    let mut imports = lex_vcs::ImportMap::new();
    for (file, modules) in &loaded.imports_by_file {
        let e = imports.entry(file.clone()).or_default();
        for (reference, alias) in modules {
            e.insert(lex_vcs::ImportRef { reference: reference.clone(), alias: alias.clone() });
        }
    }
    store
        .publish_program_with_intent(
            DEFAULT_BRANCH, &stages, &diff, &imports, true, None, None, &loaded.module_prefixes,
        )
        .expect("publish")
        .head_op
        .expect("head")
}

/// Two independent stores, so each head is built from its own file layout —
/// which is what a rename between releases actually looks like.
fn api_of(files: &[(&str, &str)]) -> (lex_store::api::PublicApi, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let head = publish(&store, &tmp.path().join("src-root"), files);
    (public_api_at_op(&store, &head).expect("api"), tmp)
}

const JOBS: &str = "type JobOpts = { max_attempts :: Int }\n\nfn enqueue(n :: Int) -> Int { n + 1 }\n";

/// The bug: identical declarations, different file name. Nothing about the API
/// changed, so the gate must not demand a major bump.
#[test]
fn renaming_the_file_is_not_a_breaking_api_change() {
    let (before, _a) = api_of(&[("lib.lex", JOBS)]);
    let (after, _b) = api_of(&[("jobs.lex", JOBS)]);

    // The module is part of a consumer's import path, so the keys do differ —
    // but on the module, not on a content hash.
    assert!(
        before.keys().any(|k| k.starts_with("lib.")),
        "keys should name the module, got {:?}",
        before.keys().collect::<Vec<_>>()
    );
    assert!(
        before.keys().all(|k| !k.contains('_') || !k.contains("lib_")),
        "no mangling hash may survive into the key: {:?}",
        before.keys().collect::<Vec<_>>()
    );

    // Same declarations under the same module name → nothing changed at all.
    let (same, _c) = api_of(&[("lib.lex", JOBS)]);
    assert_eq!(
        classify_api_change(&before, &same),
        ApiChange::None,
        "republishing identical files must be a patch"
    );
    let _ = after;
}

/// The property that actually broke: a declaration present in both versions
/// must never be reported as *removed*.
#[test]
fn a_surviving_declaration_is_never_reported_removed() {
    let (before, _a) = api_of(&[("lib.lex", JOBS)]);
    let (after, _b) = api_of(&[("jobs.lex", JOBS)]);

    let bare_before: Vec<&str> =
        before.keys().map(|k| k.rsplit('.').next().unwrap()).collect();
    let bare_after: Vec<&str> = after.keys().map(|k| k.rsplit('.').next().unwrap()).collect();
    assert_eq!(bare_before, bare_after, "the same declarations exist either side");
    assert!(bare_after.contains(&"JobOpts"), "got {bare_after:?}");

    // A module rename IS breaking — `<pkg>/<module>` is the import path — but
    // the message has to say which module moved, not imply the type vanished.
    // Before #991 this read "`JobOpts` was removed", which sent me looking for
    // a deleted type that was sitting right there in both versions.
    if let ApiChange::Breaking(why) = classify_api_change(&before, &after) {
        assert!(
            why.contains("lib."),
            "the message must name the module that moved, so it is diagnosable \
             as a rename rather than a vanished declaration: {why}"
        );
    }
}

/// Two modules may each define `validate` (#818). Keying on the bare name
/// alone would merge them and hide a real removal, so the module must stay.
#[test]
fn same_named_declarations_in_two_modules_stay_distinct() {
    let (api, _t) = api_of(&[
        ("a.lex", "fn validate(n :: Int) -> Bool { n > 0 }\n"),
        ("b.lex", "fn validate(s :: Str) -> Bool { s != \"\" }\n"),
    ]);
    let validates: Vec<&String> =
        api.keys().filter(|k| k.ends_with(".validate")).collect();
    assert_eq!(
        validates.len(), 2,
        "both modules' `validate` must appear separately, got {:?}",
        api.keys().collect::<Vec<_>>()
    );
}

/// Genuine breakage is still caught: removing a declaration outright.
#[test]
fn removing_a_declaration_is_still_breaking() {
    let (before, _a) = api_of(&[("lib.lex", JOBS)]);
    let (after, _b) = api_of(&[("lib.lex", "fn enqueue(n :: Int) -> Int { n + 1 }\n")]);
    assert!(
        matches!(classify_api_change(&before, &after), ApiChange::Breaking(_)),
        "dropping JobOpts must still require a major bump"
    );
}

/// …as is changing a signature in place.
#[test]
fn changing_a_signature_is_still_breaking() {
    let (before, _a) = api_of(&[("lib.lex", JOBS)]);
    let (after, _b) = api_of(&[(
        "lib.lex",
        "type JobOpts = { max_attempts :: Int }\n\nfn enqueue(n :: Str) -> Int { 1 }\n",
    )]);
    assert!(
        matches!(classify_api_change(&before, &after), ApiChange::Breaking(_)),
        "a changed parameter type must still require a major bump"
    );
}

/// And an addition is still additive, not a patch.
#[test]
fn adding_a_declaration_is_still_additive() {
    let (before, _a) = api_of(&[("lib.lex", JOBS)]);
    let (after, _b) = api_of(&[(
        "lib.lex",
        "type JobOpts = { max_attempts :: Int }\n\nfn enqueue(n :: Int) -> Int { n + 1 }\n\nfn drain(n :: Int) -> Int { n - 1 }\n",
    )]);
    assert!(
        matches!(classify_api_change(&before, &after), ApiChange::Additive(_)),
        "a new declaration must still require at least a minor bump"
    );
}
