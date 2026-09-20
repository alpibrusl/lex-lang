//! #942: extract **one module's** public surface from a *multi-file* package
//! head.
//!
//! `module_record_at_op` previously required a dependency to be a single
//! source file, returning `UnsupportedMultiModuleDependency` otherwise — so
//! the hub's cross-store resolver could not type-check a dependent against any
//! real multi-module package, and both de-mangled replay (#980) and the
//! non-inlined gate fell back for them. Most of the hosted libraries are
//! multi-module, so this was the cap on all of it.
//!
//! The load-bearing subtlety is the **name collision**: two files in one
//! package may each define `validate` (#818). De-mangling a multi-file head
//! wholesale would collapse them onto one bare name. So only the requested
//! module is de-mangled and its siblings keep their prefixes — which is what
//! the collision test below actually pins down.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lex_store::render::{module_record_at_op, module_record_at_op_for};
use lex_store::{Store, StoreError, DEFAULT_BRANCH};
use lex_types::Ty;

/// Build a real multi-file package on disk, load it through the same loader
/// `lex publish <dir>` uses (so the mangling is genuine, not hand-faked), and
/// publish it. Returns the head op.
fn publish_package(store: &Store, dir: &std::path::Path, files: &[(&str, &str)]) -> String {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut entries: Vec<PathBuf> = Vec::new();
    for (name, body) in files {
        let p = src.join(name);
        std::fs::write(&p, body).unwrap();
        entries.push(p);
    }
    let loaded = lex_syntax::loader::load_package(&entries, dir, "testpkg", false)
        .expect("load package");
    let stages = lex_ast::canonicalize_program(&loaded.program);

    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let empty = BTreeMap::new();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&empty, &new, &et, &et, true);

    let mut imports = lex_vcs::ImportMap::new();
    for (file, modules) in &loaded.imports_by_file {
        let entry = imports.entry(file.clone()).or_default();
        for (reference, alias) in modules {
            entry.insert(lex_vcs::ImportRef {
                reference: reference.clone(),
                alias: alias.clone(),
            });
        }
    }

    store
        .publish_program_with_intent(
            DEFAULT_BRANCH,
            &stages,
            &diff,
            &imports,
            true,
            None,
            None,
            &loaded.module_prefixes,
        )
        .expect("publish package")
        .head_op
        .expect("head op")
}

fn field_names(rec: &Ty) -> Vec<String> {
    match rec {
        Ty::Record(fs) => {
            let mut v: Vec<String> = fs.iter().map(|(n, _)| n.clone()).collect();
            v.sort();
            v
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

/// `model.lex` calls into `util.lex`, and **both** define `validate` with
/// different signatures — the #818 collision that makes wholesale de-mangling
/// wrong.
const MODEL: &str = "import \"./util\" as u\n\n\
     fn describe(n :: Int) -> Int { u.helper(n) + 1 }\n\
     fn validate(n :: Int) -> Bool { n > 0 }\n";
const UTIL: &str = "fn helper(n :: Int) -> Int { n * 2 }\n\
     fn validate(s :: Str) -> Bool { s == \"ok\" }\n";

#[test]
fn extracts_one_modules_surface_from_a_multi_file_head() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let pkg = tmp.path().join("pkg");
    let head = publish_package(&store, &pkg, &[("model.lex", MODEL), ("util.lex", UTIL)]);

    let rec = module_record_at_op_for(&store, &head, Some("model")).expect("model surface");
    assert_eq!(
        field_names(&rec),
        vec!["describe".to_string(), "validate".to_string()],
        "the record must be exactly model.lex's surface — not util's, and not the union"
    );
}

#[test]
fn a_sibling_modules_functions_are_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let pkg = tmp.path().join("pkg");
    let head = publish_package(&store, &pkg, &[("model.lex", MODEL), ("util.lex", UTIL)]);

    let rec = module_record_at_op_for(&store, &head, Some("util")).expect("util surface");
    assert_eq!(
        field_names(&rec),
        vec!["helper".to_string(), "validate".to_string()],
        "asking for `util` must yield util's surface, not model's"
    );
}

/// The collision is the point: `validate` exists in both modules with
/// different types, and each module's record must carry *its own*.
#[test]
fn colliding_names_keep_their_own_modules_signature() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let pkg = tmp.path().join("pkg");
    let head = publish_package(&store, &pkg, &[("model.lex", MODEL), ("util.lex", UTIL)]);

    let ty_of = |rec: &Ty, want: &str| -> String {
        match rec {
            Ty::Record(fs) => fs
                .iter()
                .find(|(n, _)| n.as_str() == want)
                .map(|(_, t)| format!("{t:?}"))
                .unwrap_or_else(|| panic!("no field {want}")),
            other => panic!("expected record, got {other:?}"),
        }
    };
    let model = module_record_at_op_for(&store, &head, Some("model")).expect("model");
    let util = module_record_at_op_for(&store, &head, Some("util")).expect("util");

    let m = ty_of(&model, "validate");
    let u = ty_of(&util, "validate");
    assert_ne!(
        m, u,
        "model.validate(Int)->Bool and util.validate(Str)->Bool must not collapse \
         onto one signature: model={m} util={u}"
    );
    assert!(m.contains("Int"), "model's validate takes Int, got {m}");
    assert!(u.contains("Str"), "util's validate takes Str, got {u}");
}

#[test]
fn an_unknown_module_is_reported_not_silently_wrong() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let pkg = tmp.path().join("pkg");
    let head = publish_package(&store, &pkg, &[("model.lex", MODEL), ("util.lex", UTIL)]);

    let err = module_record_at_op_for(&store, &head, Some("nope"));
    assert!(
        matches!(err, Err(StoreError::UnsupportedMultiModuleDependency)),
        "a module that isn't in the package must be an error, not an empty or \
         arbitrary record"
    );
}

/// A single-file head still works through the original entry point, with no
/// module named — the pre-#942 path must be untouched.
#[test]
fn a_single_file_head_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let pkg = tmp.path().join("pkg");
    let head = publish_package(
        &store,
        &pkg,
        &[("lib.lex", "fn gcd(a :: Int, b :: Int) -> Int { if b == 0 { a } else { gcd(b, a % b) } }\n")],
    );

    let rec = module_record_at_op(&store, &head).expect("single-file record");
    assert_eq!(field_names(&rec), vec!["gcd".to_string()]);
}
