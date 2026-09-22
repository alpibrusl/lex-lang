//! #943 / #944: recursive, lock-driven dependency resolution.
//!
//! Three real packages, each in its own store, each built by the real loader
//! without inlining (so the op-logs keep `import` edges exactly as
//! `lex publish <dir>` writes them):
//!
//! ```text
//!   A (the head under gate)  ──import──▶  B = lex-b/wrap  ──import──▶  C = lex-c/thing
//! ```
//!
//! B's *signature* names C's type (`fn wrap(n) -> c.Thing`), and A reads a
//! field of that type — so A only checks if B's surface was resolved with C
//! resolved **through B's own committed lock**, and C's type declarations
//! travel with it. Negative controls pin down that each piece is load-bearing:
//! drop B's lock and A fails with a structured `unpinned_dependency`; drop A's
//! pin and A fails the same way instead of `unknown_identifier`; a cycle
//! terminates; a diamond whose sides disagree is flagged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lex_store::deps::{module_surface_at_op_with, resolve_with_lock, DepLocator};
use lex_store::{DepResolver, ResolvedDeps, Store, StoreError, DEFAULT_BRANCH};
use lex_syntax::lock::{LockEntry, LockFile};
use lex_types::TypeError;

/// `test/<pkg>` → `<root>/<pkg>`. The registry string is otherwise opaque.
struct DirLocator {
    root: PathBuf,
}

impl DepLocator for DirLocator {
    fn open(&self, entry: &LockEntry) -> Result<(String, Store), String> {
        let name = entry
            .registry
            .strip_prefix("test/")
            .ok_or("not a test registry")?;
        let dir = self.root.join(name);
        if !dir.is_dir() {
            return Err(format!("no store for `{}`", entry.registry));
        }
        let store = Store::open(&dir).map_err(|e| e.to_string())?;
        Ok((dir.display().to_string(), store))
    }
}

/// The resolver a client installs for `lex publish`: resolves against a given
/// ("working-copy") lock.
struct FixedLockResolver {
    locator: DirLocator,
    lock: Option<LockFile>,
}

impl DepResolver for FixedLockResolver {
    fn resolve_modules(
        &self,
        s: &[lex_ast::Stage],
        h: Option<&str>,
    ) -> BTreeMap<String, lex_types::Ty> {
        self.resolve(s, h).modules
    }
    fn resolve(&self, stages: &[lex_ast::Stage], _head: Option<&str>) -> ResolvedDeps {
        resolve_with_lock(&self.locator, stages, self.lock.as_ref())
    }
}

fn lock(pins: &[(&str, &str)]) -> LockFile {
    let with_store: Vec<(&str, &str, &str)> = pins.iter().map(|(n, h)| (*n, *n, *h)).collect();
    lock_at(&with_store)
}

/// Pins `(package, store, head)` — the store may differ from the package name
/// (two releases of one package hosted side by side).
fn lock_at(pins: &[(&str, &str, &str)]) -> LockFile {
    LockFile {
        version: 1,
        packages: pins
            .iter()
            .map(|(name, store, head)| LockEntry {
                name: name.to_string(),
                registry: format!("test/{store}"),
                constraint: "^1.0".into(),
                version: "1.0.0".into(),
                head_op: Some(head.to_string()),
            })
            .collect(),
    }
}

/// Load `files` as package `name` (no inlining) and publish into store
/// `stores/<store>`. With `pins`, the store's gate resolves through that
/// working-copy lock, and when `commit` is set the lock is committed to the
/// new head as `lex publish` + push would. Without `pins` no resolver is
/// installed (a dependency-free package, or an edge nobody calls yet).
fn publish_into(
    root: &Path,
    name: &str,
    store_name: &str,
    files: &[(&str, &str)],
    pins: Option<LockFile>,
    commit: bool,
) -> Result<String, StoreError> {
    let dir = root.join("src-trees").join(store_name);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let entries: Vec<PathBuf> = files
        .iter()
        .map(|(f, body)| {
            let p = src.join(f);
            std::fs::write(&p, body).unwrap();
            p
        })
        .collect();
    let loaded = lex_syntax::load_package(&entries, &dir, name, false).expect("load package");
    let stages = lex_ast::canonicalize_program(&loaded.program);
    let fns: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|s| match s {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let types: BTreeMap<String, lex_ast::TypeDecl> = stages
        .iter()
        .filter_map(|s| match s {
            lex_ast::Stage::TypeDecl(td) => Some((td.name.clone(), td.clone())),
            _ => None,
        })
        .collect();
    let diff =
        lex_vcs::compute_diff_with_types(&BTreeMap::new(), &fns, &BTreeMap::new(), &types, true);
    let mut imports = lex_vcs::ImportMap::new();
    for (file, modules) in &loaded.imports_by_file {
        let e = imports.entry(file.clone()).or_default();
        for (reference, alias) in modules {
            e.insert(lex_vcs::ImportRef {
                reference: reference.clone(),
                alias: alias.clone(),
            });
        }
    }
    let store_dir = root.join("stores").join(store_name);
    std::fs::create_dir_all(&store_dir).unwrap();
    let mut store = Store::open(&store_dir).unwrap();
    if let Some(l) = &pins {
        store.set_dep_resolver(Arc::new(FixedLockResolver {
            locator: DirLocator {
                root: root.join("stores"),
            },
            lock: Some(l.clone()),
        }));
    }
    let head = store
        .publish_program_with_intent(
            DEFAULT_BRANCH,
            &stages,
            &diff,
            &imports,
            true,
            None,
            None,
            &loaded.module_prefixes,
        )?
        .head_op
        .expect("head op");
    if let (Some(l), true) = (pins, commit) {
        store
            .set_committed_lock(&head, &l.to_toml().unwrap())
            .unwrap();
    }
    Ok(head)
}

fn publish(
    root: &Path,
    name: &str,
    files: &[(&str, &str)],
    pins: Option<LockFile>,
) -> Result<String, StoreError> {
    publish_into(root, name, name, files, pins, true)
}

const C_THING: &str = "type Thing = { n :: Int }\n\nfn make(n :: Int) -> Thing { { n: n } }\n";
const B_WRAP: &str = "import \"lex-c/thing\" as c\n\ntype Wrapper = { t :: c.Thing }\n\n\
fn wrap(n :: Int) -> c.Thing { c.make(n + 1) }\n\n\
fn boxed(n :: Int) -> Wrapper { { t: c.make(n) } }\n";

/// A's head: reads a field of C's type through B's signature — resolvable
/// only if C's type declarations travelled with B's surface.
fn a_stages() -> Vec<lex_ast::Stage> {
    let src = "import \"lex-b/wrap\" as b\n\n\
fn use_it() -> Int { b.wrap(3).n }\n\n\
fn use_box() -> Int { b.boxed(4).t.n }\n";
    let prog = lex_syntax::parse_source(src).expect("parse A");
    lex_ast::canonicalize_program(&prog)
}

struct Chain {
    tmp: tempfile::TempDir,
    c_head: String,
    b_head: String,
}

impl Chain {
    fn locator(&self) -> DirLocator {
        DirLocator {
            root: self.tmp.path().join("stores"),
        }
    }
}

fn chain() -> Chain {
    let tmp = tempfile::tempdir().unwrap();
    let c_head = publish(tmp.path(), "lex-c", &[("thing.lex", C_THING)], None).expect("publish C");
    let b_head = publish(
        tmp.path(),
        "lex-b",
        &[("wrap.lex", B_WRAP)],
        Some(lock(&[("lex-c", &c_head)])),
    )
    .expect("publish B (its gate resolves C through the working-copy lock)");
    Chain {
        tmp,
        c_head,
        b_head,
    }
}

fn check(stages: &[lex_ast::Stage], deps: &ResolvedDeps) -> Result<(), Vec<TypeError>> {
    if !deps.diagnostics.is_empty() {
        return Err(deps.diagnostics.clone());
    }
    lex_types::check_program_with_deps(stages, &deps.modules, &deps.types, &deps.prefixes)
        .map(|_| ())
}

fn kinds(errs: &[TypeError]) -> Vec<String> {
    errs.iter()
        .map(|e| {
            serde_json::to_value(e).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn three_store_chain_resolves_through_each_dependencys_own_lock() {
    let ch = chain();
    let deps = resolve_with_lock(
        &ch.locator(),
        &a_stages(),
        Some(&lock(&[("lex-b", &ch.b_head)])),
    );
    assert!(
        deps.diagnostics.is_empty(),
        "unexpected diagnostics: {:?}",
        deps.diagnostics
    );
    check(&a_stages(), &deps).expect("A must check against B, with B's C resolved via B's lock");
    // The record is B's own surface, bare-keyed.
    match deps.modules.get("lex-b/wrap").expect("B resolved") {
        lex_types::Ty::Record(fs) => {
            let mut names: Vec<&String> = fs.keys().collect();
            names.sort();
            assert_eq!(names, vec!["boxed", "wrap"]);
        }
        other => panic!("expected a record, got {other:?}"),
    }
    // C's type travelled with B's surface, re-prefixed by C's package identity.
    let c_prefix = lex_syntax::package_file_prefix("lex-c", "src/thing.lex");
    let names: Vec<&str> = deps.types["lex-b/wrap"]
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert!(
        names.contains(&format!("{c_prefix}.Thing").as_str()),
        "{names:?}"
    );
    assert_eq!(
        deps.prefixes["lex-b/wrap"],
        lex_syntax::package_file_prefix("lex-b", "src/wrap.lex")
    );
}

/// Negative control for the positive test above: A misusing C's type must
/// still be rejected — the resolution isn't making anything typecheck.
#[test]
fn a_wrong_use_of_the_transitive_type_is_still_rejected() {
    let ch = chain();
    let src = "import \"lex-b/wrap\" as b\n\nfn bad() -> Str { b.wrap(3).n }\n";
    let stages = lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap());
    let deps = resolve_with_lock(
        &ch.locator(),
        &stages,
        Some(&lock(&[("lex-b", &ch.b_head)])),
    );
    assert!(deps.diagnostics.is_empty(), "{:?}", deps.diagnostics);
    assert!(
        check(&stages, &deps).is_err(),
        "Int field returned as Str must not check"
    );
}

#[test]
fn removing_bs_lock_makes_a_fail_with_unpinned_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let c_head = publish(tmp.path(), "lex-c", &[("thing.lex", C_THING)], None).unwrap();
    // Publish B with a working-copy lock (so its own publish gate passes) but
    // commit NONE: a hosted consumer then cannot know which C it meant.
    let dir = tmp.path().join("stores");
    let b_head = publish_into(
        tmp.path(),
        "lex-b",
        "lex-b",
        &[("wrap.lex", B_WRAP)],
        Some(lock(&[("lex-c", &c_head)])),
        false,
    )
    .unwrap();
    assert_eq!(
        Store::open(dir.join("lex-b"))
            .unwrap()
            .committed_lock_inherited(&b_head)
            .unwrap(),
        None,
        "B must carry no committed lock for this control"
    );
    let deps = resolve_with_lock(
        &DirLocator { root: dir },
        &a_stages(),
        Some(&lock(&[("lex-b", &b_head)])),
    );
    assert_eq!(
        kinds(&deps.diagnostics),
        vec!["unresolved_dependency"],
        "{:?}",
        deps.diagnostics
    );
    let msg = deps.diagnostics[0].to_string();
    assert!(
        msg.contains("unpinned dependency") && msg.contains("lex-c/thing"),
        "inner cause must name C's import: {msg}"
    );
    assert!(!deps.modules.contains_key("lex-b/wrap"));
}

#[test]
fn an_import_the_root_lock_does_not_pin_is_unpinned_not_unknown_identifier() {
    let ch = chain();
    // A lock that pins something else entirely (a git-only dep has no entry).
    let deps = resolve_with_lock(
        &ch.locator(),
        &a_stages(),
        Some(&lock(&[("lex-c", &ch.c_head)])),
    );
    assert_eq!(kinds(&deps.diagnostics), vec!["unpinned_dependency"]);
    let v = serde_json::to_value(&deps.diagnostics[0]).unwrap();
    assert_eq!(v["reference"], "lex-b/wrap");
    assert_eq!(v["package"], "lex-b");
    assert!(v["hint"].as_str().unwrap().contains("lex pkg lock"));
    // No lock at all: same diagnostic.
    let deps = resolve_with_lock(&ch.locator(), &a_stages(), None);
    assert_eq!(kinds(&deps.diagnostics), vec!["unpinned_dependency"]);
}

/// The store gate reports the structured diagnostic (not `unknown_identifier`)
/// and refuses the head.
#[test]
fn the_write_time_gate_reports_unpinned_dependency() {
    let ch = chain();
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path())
        .unwrap()
        .with_dep_resolver(Arc::new(FixedLockResolver {
            locator: ch.locator(),
            lock: None,
        }));
    let stages = a_stages();
    let fns: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|s| match s {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let diff = lex_vcs::compute_diff(&BTreeMap::new(), &fns, true);
    let err = store
        .publish_program(
            DEFAULT_BRANCH,
            &stages,
            &diff,
            &lex_vcs::ImportMap::new(),
            true,
        )
        .expect_err("an unpinned import must fail the gate");
    let StoreError::TypeError(errs) = err else {
        panic!("expected type errors, got {err:?}")
    };
    assert_eq!(kinds(&errs), vec!["unpinned_dependency"], "the diagnostic replaces unknown_identifier: {errs:?}");
}

#[test]
fn a_dependency_cycle_terminates_with_a_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stores");
    // X imports Y and Y imports X. Publish both unchecked-by-deps first (the
    // publish gate needs no resolver for the edge because neither calls the
    // other), then commit locks pointing at each other.
    let x = publish(
        tmp.path(),
        "lex-x",
        &[("x.lex", "import \"lex-y/y\" as y\n\nfn fx() -> Int { 1 }\n")],
        None,
    )
    .unwrap();
    let y = publish(
        tmp.path(),
        "lex-y",
        &[("y.lex", "import \"lex-x/x\" as x\n\nfn fy() -> Int { 2 }\n")],
        None,
    )
    .unwrap();
    Store::open(root.join("lex-x"))
        .unwrap()
        .set_committed_lock(&x, &lock(&[("lex-y", &y)]).to_toml().unwrap())
        .unwrap();
    Store::open(root.join("lex-y"))
        .unwrap()
        .set_committed_lock(&y, &lock(&[("lex-x", &x)]).to_toml().unwrap())
        .unwrap();

    let src = "import \"lex-x/x\" as x\n\nfn f() -> Int { x.fx() }\n";
    let stages = lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap());
    let deps = resolve_with_lock(&DirLocator { root }, &stages, Some(&lock(&[("lex-x", &x)])));
    assert_eq!(kinds(&deps.diagnostics), vec!["unresolved_dependency"]);
    assert!(
        deps.diagnostics[0].to_string().contains("cycle"),
        "{}",
        deps.diagnostics[0]
    );
}

#[test]
fn a_diamond_whose_sides_disagree_is_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let c1 = publish(tmp.path(), "lex-c", &[("thing.lex", C_THING)], None).unwrap();
    // A second release of the same package, hosted side by side.
    let more = format!("{C_THING}\nfn extra() -> Int {{ 7 }}\n");
    let c2 = publish_into(
        tmp.path(),
        "lex-c",
        "lex-c-v2",
        &[("thing.lex", &more)],
        None,
        false,
    )
    .unwrap();
    assert_ne!(c1, c2);
    let b = publish(
        tmp.path(),
        "lex-b",
        &[("wrap.lex", B_WRAP)],
        Some(lock(&[("lex-c", &c1)])),
    )
    .unwrap();
    let locator = DirLocator {
        root: tmp.path().join("stores"),
    };
    let src = "import \"lex-b/wrap\" as b\nimport \"lex-c/thing\" as c\n\nfn f() -> Int { b.wrap(1).n }\n";
    let stages = lex_ast::canonicalize_program(&lex_syntax::parse_source(src).unwrap());

    // Root pins C at c2 while B pins it at c1: conflict.
    let deps = resolve_with_lock(
        &locator,
        &stages,
        Some(&lock_at(&[
            ("lex-b", "lex-b", &b),
            ("lex-c", "lex-c-v2", &c2),
        ])),
    );
    assert_eq!(
        kinds(&deps.diagnostics),
        vec!["dependency_conflict"],
        "{:?}",
        deps.diagnostics
    );
    // Control: agreeing pins are not a conflict, and the diamond checks.
    let deps = resolve_with_lock(
        &locator,
        &stages,
        Some(&lock(&[("lex-b", &b), ("lex-c", &c1)])),
    );
    assert!(deps.diagnostics.is_empty(), "{:?}", deps.diagnostics);
    check(&stages, &deps).expect("a consistent diamond must check");
}

/// Two packages that both ship `src/error.lex` get distinct prefixes by
/// package identity, however the op-logs were published — here both were
/// published under the SAME mangling namespace (as legacy path-only publishes
/// effectively were), so their stored prefixes collide.
#[test]
fn same_layout_in_two_packages_gets_distinct_prefixes() {
    let tmp = tempfile::tempdir().unwrap();
    let body = "type Err = { msg :: Str }\n\nfn mk(m :: Str) -> Err { { msg: m } }\n";
    let s = publish_into(
        tmp.path(),
        "legacy",
        "lex-schema",
        &[("error.lex", body)],
        None,
        false,
    )
    .unwrap();
    let o = publish_into(
        tmp.path(),
        "legacy",
        "lex-orm",
        &[("error.lex", body)],
        None,
        false,
    )
    .unwrap();
    let root = tmp.path().join("stores");
    let ss = module_surface_at_op_with(
        &Store::open(root.join("lex-schema")).unwrap(),
        &s,
        "lex-schema",
        "error",
        &ResolvedDeps::default(),
    )
    .unwrap();
    let os = module_surface_at_op_with(
        &Store::open(root.join("lex-orm")).unwrap(),
        &o,
        "lex-orm",
        "error",
        &ResolvedDeps::default(),
    )
    .unwrap();
    assert_ne!(ss.prefix, os.prefix);
    let want = lex_syntax::package_file_prefix("lex-schema", "src/error.lex");
    assert_eq!(ss.prefix.as_deref(), Some(want.as_str()));
    // The exported type is renamed with it (not left under the stored prefix).
    assert_eq!(
        ss.types.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
        vec![format!("{want}.Err")]
    );
}
