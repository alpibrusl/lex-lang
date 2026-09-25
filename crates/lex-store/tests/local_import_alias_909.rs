//! #909: a local import's alias (`import "./error" as e`) is recorded in the
//! op-log as an `AddImport` and rendered back verbatim.
//!
//! An `AddImport` whose module is a **path** (`./error`) is a new shape in the
//! op-log. Until now every recorded import was a stdlib module or a package, and
//! several readers assume exactly that — anything that turns a head's imports
//! into `Stage::Import`s hands them to a type-check gate, a dependency resolver,
//! or a program loader. A `./error` reaching any of those would be read as a
//! registry package (`unpinned_dependency`) or resolved relative to the wrong
//! directory. So besides the render round trip these tests pin every reader
//! that must keep local imports OUT of the head's import *edges*.
//!
//! Each key test was mutation-checked: reverting the corresponding fix (the
//! loader recording, the renderer preferring the recorded alias, or
//! `PackageHead::add_import` keeping locals out of `flat_imports`) makes it
//! fail.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use lex_store::render::{
    module_record_at_op_for, package_head_at_op, render_source, PackageHead, RenderedSource,
};
use lex_store::{DepResolver, Store, DEFAULT_BRANCH};
use lex_types::{module_record_from_fields, EffectSet, Ty};

const ERROR: &str = "type Err = { code :: Int, msg :: Str }\n\n\
    fn format(e :: Err) -> Str {\n  e.msg\n}\n";

/// Records every import reference a gate hands the resolver, and supplies
/// `lex-nt/lib` so a genuine package import next to a local one still resolves.
#[derive(Default)]
struct SpyResolver {
    seen: Mutex<Vec<String>>,
}
impl DepResolver for SpyResolver {
    fn resolve_modules(
        &self,
        stages: &[lex_ast::Stage],
        _head_op: Option<&str>,
    ) -> BTreeMap<String, Ty> {
        let mut seen = self.seen.lock().unwrap();
        for s in stages {
            if let lex_ast::Stage::Import(i) = s {
                seen.push(i.reference.clone());
            }
        }
        let rec = module_record_from_fields(vec![(
            "gcd".to_string(),
            Ty::function(vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int()),
        )]);
        let mut m = BTreeMap::new();
        m.insert("lex-nt/lib".to_string(), rec);
        m
    }
}

/// Write `files` (relative to `<pkg>/src/`) and load them exactly as
/// `lex publish <dir>` does (non-inlined, real mangling).
fn load(pkg: &Path, files: &[(&str, &str)]) -> lex_syntax::loader::LoadedPackage {
    let src = pkg.join("src");
    let mut entries: Vec<PathBuf> = Vec::new();
    for (name, body) in files {
        let p = src.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        entries.push(p);
    }
    entries.sort();
    lex_syntax::loader::load_package(&entries, pkg, "testpkg", false).expect("load package")
}

/// Publish `pkg` into `store`. `record_local: false` reproduces what a
/// pre-#909 publish wrote — the local imports simply absent from the import
/// map — i.e. an OLD log.
fn publish_with(
    store: &Store,
    pkg: &Path,
    files: &[(&str, &str)],
    record_local: bool,
) -> lex_store::PublishOutcome {
    let loaded = load(pkg, files);
    let stages = lex_ast::canonicalize_program(&loaded.program);
    let mut new_fns: BTreeMap<String, lex_ast::FnDecl> = BTreeMap::new();
    let mut new_types: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    for st in &stages {
        match st {
            lex_ast::Stage::FnDecl(fd) => {
                new_fns.insert(fd.name.clone(), fd.clone());
            }
            lex_ast::Stage::TypeDecl(td) => {
                new_types.insert(td.name.clone(), td.clone());
            }
            _ => {}
        }
    }
    // Diff against the current head, as the CLI does.
    let head = store.branch_head(DEFAULT_BRANCH).unwrap();
    let pairs: Vec<(String, String)> = head.into_iter().collect();
    let mut old_fns = BTreeMap::new();
    let mut old_types = BTreeMap::new();
    for ast in store.get_asts_for_sigs_bulk(&pairs).into_iter().flatten() {
        match ast {
            lex_ast::Stage::FnDecl(fd) => {
                old_fns.insert(fd.name.clone(), fd);
            }
            lex_ast::Stage::TypeDecl(td) => {
                old_types.insert(td.name.clone(), td);
            }
            _ => {}
        }
    }
    let diff = lex_vcs::compute_diff_with_types(&old_fns, &new_fns, &old_types, &new_types, true);

    let mut imports = lex_vcs::ImportMap::new();
    for (file, modules) in &loaded.imports_by_file {
        let entry = imports.entry(file.clone()).or_default();
        for (reference, alias) in modules {
            if !record_local && lex_vcs::is_local_import(reference) {
                continue;
            }
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
}

fn publish(store: &Store, pkg: &Path, files: &[(&str, &str)]) -> lex_store::PublishOutcome {
    publish_with(store, pkg, files, true)
}

fn head_of(store: &Store) -> String {
    store
        .get_branch(DEFAULT_BRANCH)
        .unwrap()
        .and_then(|b| b.head_op)
        .expect("a head")
}

fn tree(store: &Store) -> BTreeMap<String, String> {
    let head = package_head_at_op(store, &head_of(store)).expect("head");
    match render_source(store, &head).expect("render") {
        RenderedSource::Multi(t) => t,
        other => panic!("expected a multi-file tree, got {other:?}"),
    }
}

fn add_imports(o: &lex_store::PublishOutcome) -> Vec<(String, String, Option<String>)> {
    o.ops
        .iter()
        .filter(|p| p.kind["op"] == "add_import")
        .map(|p| {
            (
                p.kind["in_file"].as_str().unwrap().to_string(),
                p.kind["module"].as_str().unwrap().to_string(),
                p.kind.get("alias").and_then(|a| a.as_str()).map(str::to_string),
            )
        })
        .collect()
}

fn fixture() -> (tempfile::TempDir, Store, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let pkg = tmp.path().join("pkg");
    (tmp, store, pkg)
}

const JSON_VALUE: &str = "import \"./error\" as e\n\n\
    fn render(x :: e.Err) -> Str {\n  e.format(x)\n}\n";

// ── round trip ────────────────────────────────────────────────────────────

#[test]
fn a_non_default_local_alias_round_trips_and_is_recorded_once() {
    let (_t, store, pkg) = fixture();
    let out = publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);

    // Recorded as an AddImport carrying the alias.
    assert_eq!(
        add_imports(&out),
        vec![("src/json_value.lex".into(), "./error".into(), Some("e".into()))],
    );

    let t = tree(&store);
    assert_eq!(t["src/json_value.lex"], JSON_VALUE, "verbatim, modulo canonical print");
}

#[test]
fn a_default_alias_local_import_emits_no_alias_field_and_round_trips() {
    let (_t, store, pkg) = fixture();
    let src = "import \"./error\" as error\n\n\
        fn render(x :: error.Err) -> Str {\n  error.format(x)\n}\n";
    let out = publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", src)]);
    assert_eq!(
        add_imports(&out),
        vec![("src/json_value.lex".into(), "./error".into(), None)],
        "the default alias must not be stored (OpId stability, #895)"
    );
    // The serialized op has no `alias` key at all.
    let op = out.ops.iter().find(|p| p.kind["op"] == "add_import").unwrap();
    assert!(op.kind.get("alias").is_none(), "{}", op.kind);
    assert_eq!(tree(&store)["src/json_value.lex"], src);
}

#[test]
fn two_files_importing_one_target_under_different_aliases_stay_distinct() {
    let (_t, store, pkg) = fixture();
    let a = "import \"./error\" as e\n\nfn a(x :: e.Err) -> Str {\n  e.format(x)\n}\n";
    let b = "import \"./error\" as err\n\nfn b(x :: err.Err) -> Str {\n  err.format(x)\n}\n";
    publish(&store, &pkg, &[("error.lex", ERROR), ("a.lex", a), ("b.lex", b)]);
    let t = tree(&store);
    assert_eq!(t["src/a.lex"], a);
    assert_eq!(t["src/b.lex"], b);
}

#[test]
fn one_file_importing_two_local_modules_keeps_both_aliases() {
    let (_t, store, pkg) = fixture();
    let util = "fn twice(n :: Int) -> Int {\n  n * 2\n}\n";
    // (The canonical printer puts a blank line after each import and sorts the
    // declarations, so the source is written the way it prints.)
    let main = "import \"./error\" as e\n\n\
        import \"./util\" as u\n\n\
        fn go(n :: Int) -> Int {\n  u.twice(n)\n}\n\n\
        fn run(x :: e.Err) -> Str {\n  e.format(x)\n}\n";
    publish(&store, &pkg, &[("error.lex", ERROR), ("util.lex", util), ("main.lex", main)]);
    assert_eq!(tree(&store)["src/main.lex"], main);
}

#[test]
fn a_nested_directory_local_import_round_trips_from_both_sides() {
    let (_t, store, pkg) = fixture();
    let strings = "fn shout(s :: Str) -> Str {\n  s\n}\n";
    // Down into a subdirectory...
    let main = "import \"./util/strings\" as s\n\nfn run(x :: Str) -> Str {\n  s.shout(x)\n}\n";
    // ...and back up out of one.
    let deep = "import \"../error\" as err\n\nfn f(x :: err.Err) -> Str {\n  err.format(x)\n}\n";
    publish(
        &store,
        &pkg,
        &[
            ("error.lex", ERROR),
            ("util/strings.lex", strings),
            ("util/deep.lex", deep),
            ("main.lex", main),
        ],
    );
    let t = tree(&store);
    assert_eq!(t["src/main.lex"], main);
    assert_eq!(t["src/util/deep.lex"], deep);
}

/// The old derivation had to dodge a param named like the module's stem by
/// falling back to the mangle prefix; the recorded alias is simply what the
/// source wrote.
#[test]
fn an_alias_the_derivation_would_have_had_to_dodge_is_kept_verbatim() {
    let (_t, store, pkg) = fixture();
    let util = "fn helper(x :: Int) -> Int {\n  x + 1\n}\n";
    let main = "import \"./util\" as u\n\nfn run(util :: Int) -> Int {\n  u.helper(util)\n}\n";
    publish(&store, &pkg, &[("util.lex", util), ("main.lex", main)]);
    assert_eq!(tree(&store)["src/main.lex"], main);
}

// ── evolution ─────────────────────────────────────────────────────────────

#[test]
fn changing_an_alias_between_publishes_removes_and_adds_and_the_render_follows() {
    let (_t, store, pkg) = fixture();
    publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);

    // Only the alias changes — the mangled program is identical, so the ONLY
    // ops are the import pair.
    let renamed = "import \"./error\" as err\n\n\
        fn render(x :: err.Err) -> Str {\n  err.format(x)\n}\n";
    let out = publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", renamed)]);
    let kinds: Vec<&str> = out.ops.iter().map(|p| p.kind["op"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["remove_import", "add_import"], "{:?}", out.ops);
    assert_eq!(
        add_imports(&out),
        vec![("src/json_value.lex".into(), "./error".into(), Some("err".into()))]
    );
    assert_eq!(tree(&store)["src/json_value.lex"], renamed);
}

#[test]
fn an_unchanged_republish_creates_no_ops() {
    let (_t, store, pkg) = fixture();
    let files = [("error.lex", ERROR), ("json_value.lex", JSON_VALUE)];
    publish(&store, &pkg, &files);
    let head = head_of(&store);
    let again = publish(&store, &pkg, &files);
    assert!(again.ops.is_empty(), "unchanged republish must be a no-op: {:?}", again.ops);
    assert_eq!(head_of(&store), head);
}

// ── old logs ──────────────────────────────────────────────────────────────

/// A log written before #909 has no local AddImports: it exports through the
/// stem/prefix derivation exactly as it always did.
#[test]
fn an_old_log_renders_through_the_derived_alias_fallback() {
    let (_t, store, pkg) = fixture();
    let out = publish_with(
        &store,
        &pkg,
        &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)],
        false,
    );
    assert!(add_imports(&out).is_empty(), "an old log records no local imports");
    // Pinned literal: this is byte-for-byte what the pre-#909 renderer produced
    // for this fixture (asserted against `main` while writing the test), which
    // is the point — `as e` is unrecoverable, the derived `error` stands.
    assert_eq!(
        tree(&store)["src/json_value.lex"],
        "import \"./error\" as error\n\n\
         fn render(x :: error.Err) -> Str {\n  error.format(x)\n}\n"
    );
}

/// Republishing a package that was published BEFORE #909 adds the missing local
/// imports once — as import-only ops, leaving every declaration alone — and the
/// next republish is a no-op again.
#[test]
fn republishing_an_old_log_backfills_the_local_imports_once() {
    let (_t, store, pkg) = fixture();
    let files = [("error.lex", ERROR), ("json_value.lex", JSON_VALUE)];
    publish_with(&store, &pkg, &files, false);
    let old_map = package_head_at_op(&store, &head_of(&store)).unwrap().map;

    let backfill = publish(&store, &pkg, &files);
    let kinds: Vec<&str> = backfill.ops.iter().map(|p| p.kind["op"].as_str().unwrap()).collect();
    assert_eq!(kinds, vec!["add_import"], "only the import is backfilled: {:?}", backfill.ops);
    assert_eq!(
        add_imports(&backfill),
        vec![("src/json_value.lex".into(), "./error".into(), Some("e".into()))]
    );
    // Head still valid, declarations untouched, render now faithful.
    let new_head = package_head_at_op(&store, &head_of(&store)).unwrap();
    assert_eq!(new_head.map, old_map, "no declaration changed");
    assert_eq!(tree(&store)["src/json_value.lex"], JSON_VALUE);
    let verdict = store
        .verify_head_and_attest(DEFAULT_BRANCH, None, &head_of(&store))
        .expect("verify");
    assert!(verdict.passed, "always-valid HEAD after the backfill: {:?}", verdict.detail);

    // And it is once.
    let again = publish(&store, &pkg, &files);
    assert!(again.ops.is_empty(), "second republish must be a no-op: {:?}", again.ops);
}

// ── every reader of a head's imports ──────────────────────────────────────

/// `PackageHead::add_import` keeps a local import out of `flat_imports` (the map
/// every gate turns into `Stage::Import`s) but records it per file.
#[test]
fn a_local_import_is_per_file_metadata_never_a_flat_import_edge() {
    let (_t, store, pkg) = fixture();
    let main = "import \"std.int\" as int\nimport \"./error\" as e\n\n\
        fn run(x :: e.Err, n :: Int) -> Str {\n  e.format(x)\n}\n";
    publish(&store, &pkg, &[("error.lex", ERROR), ("main.lex", main)]);
    let ph = package_head_at_op(&store, &head_of(&store)).unwrap();
    assert_eq!(
        ph.flat_imports.keys().cloned().collect::<Vec<_>>(),
        vec!["std.int".to_string()],
        "flat imports must not carry the local import"
    );
    assert_eq!(ph.file_imports["src/main.lex"]["./error"], "e");

    // The unit-level contract, including RemoveImport.
    let mut h = PackageHead::default();
    h.add_import("src/a.lex", "./error", Some("e"));
    h.add_import("src/a.lex", "std.io", None);
    assert_eq!(h.flat_imports.len(), 1);
    assert_eq!(h.file_imports["src/a.lex"].len(), 2);
    h.remove_import("src/a.lex", "./error");
    assert_eq!(h.file_imports["src/a.lex"].keys().collect::<Vec<_>>(), vec!["std.io"]);
}

/// Head reconstruction (replay context, #946) prepends the head's import edges
/// to the program. A local import there would be a `Stage::Import` naming a
/// path that resolves against nothing.
#[test]
fn head_reconstruction_carries_no_local_import_stage() {
    let (_t, store, pkg) = fixture();
    publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);
    let head = head_of(&store);
    for stages in [
        store.program_stages_at_op(&head).unwrap(),
        store.demangled_program_at_op(&head).unwrap(),
    ] {
        let imports: Vec<&str> = stages
            .iter()
            .filter_map(|s| match s {
                lex_ast::Stage::Import(i) => Some(i.reference.as_str()),
                _ => None,
            })
            .collect();
        assert!(imports.is_empty(), "no import stage expected, got {imports:?}");
    }
}

/// The type-check gates (publish, hub verify) and the resolver behind them
/// never see a path import, even next to a real package import — so it cannot
/// be read as a registry package and trip `unpinned_dependency`.
#[test]
fn gates_and_resolvers_never_see_a_local_import() {
    let tmp = tempfile::tempdir().unwrap();
    let spy = Arc::new(SpyResolver::default());
    let store = Store::open(tmp.path().join("store")).unwrap().with_dep_resolver(spy.clone());
    let pkg = tmp.path().join("pkg");
    let main = "import \"lex-nt/lib\" as nt\nimport \"./error\" as e\n\n\
        fn run(x :: e.Err) -> Int {\n  nt.gcd(1, 2)\n}\n";
    let out = publish(&store, &pkg, &[("error.lex", ERROR), ("main.lex", main)]);
    // Both imports were recorded...
    let recorded: BTreeSet<String> = add_imports(&out).into_iter().map(|(_, m, _)| m).collect();
    assert_eq!(recorded, ["./error".to_string(), "lex-nt/lib".to_string()].into());

    // ...and every gate that reconstructs the head still passes.
    let head = head_of(&store);
    let verdict = store.verify_head_and_attest(DEFAULT_BRANCH, None, &head).expect("verify");
    assert!(verdict.passed, "hub verify: {:?}", verdict.detail);
    // An unrelated publish exercises the write-time gates (`apply_operation`
    // reconstructs the head through `with_head_imports`).
    let main2 = format!("{main}\nfn more(n :: Int) -> Int {{\n  n + 1\n}}\n");
    publish(&store, &pkg, &[("error.lex", ERROR), ("main.lex", &main2)]);
    let _ = store.program_stages_at_op(&head_of(&store)).unwrap();

    let seen = spy.seen.lock().unwrap();
    assert!(!seen.is_empty(), "the resolver was consulted for the package import");
    assert!(
        seen.iter().all(|r| !lex_vcs::is_local_import(r)),
        "a local import reached the dependency resolver: {seen:?}"
    );
}

/// #942: per-module surface extraction de-mangles one file of a multi-file head
/// and type-checks it — with a local alias import present.
#[test]
fn per_module_surface_extraction_ignores_local_alias_imports() {
    let (_t, store, pkg) = fixture();
    publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);
    let head = head_of(&store);
    for module in ["json_value", "error"] {
        module_record_at_op_for(&store, &head, Some(module))
            .unwrap_or_else(|e| panic!("surface of {module}: {e}"));
    }
}

/// A local import is not an external dependency.
#[test]
fn a_local_import_is_not_an_external_dependency() {
    let (_t, store, pkg) = fixture();
    publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);
    let deps = lex_store::api::external_dependencies_at_op(&store, &head_of(&store)).unwrap();
    assert!(deps.is_empty(), "{deps:?}");
    // And it does not disturb the public-API view.
    lex_store::api::public_api_at_op(&store, &head_of(&store)).unwrap();
}

/// Replaying an op of a multi-file package whose files have aliased local
/// imports: the request is built (the head reconstruction runs) without
/// leaking a path import, and the recorded stage reproduces exactly.
#[test]
fn replay_of_an_op_in_a_package_with_aliased_local_imports_reproduces() {
    let (_t, store, pkg) = fixture();
    let out = publish(&store, &pkg, &[("error.lex", ERROR), ("json_value.lex", JSON_VALUE)]);
    let head = package_head_at_op(&store, &head_of(&store)).unwrap();
    let add_fn = out
        .ops
        .iter()
        .find(|p| p.kind["op"] == "add_function" && p.kind["in_file"] == "src/json_value.lex")
        .expect("json_value's add_function op");

    let req = store.replay_request(&add_fn.op_id).expect("replay request");
    assert!(
        !req.parent_program.contains("./error"),
        "the reconstructed parent program must not carry the local import:\n{}",
        req.parent_program
    );

    // The recorded stage, read back from the store, is a perfect regeneration.
    let sig = req.target_sig.clone();
    let stage_id = head.map[&sig].clone();
    let stage = store
        .get_asts_for_sigs_bulk(&[(sig, stage_id)])
        .pop()
        .unwrap()
        .expect("recorded stage");
    let outcome = store.replay_compare(&add_fn.op_id, &stage).expect("replay");
    assert!(outcome.reproduced, "{outcome:?}");
}
