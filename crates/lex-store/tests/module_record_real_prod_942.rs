//! #942 against **real hosted data**: extract one module's surface from
//! `lex-official/lex-schema`, which is a genuine 21-file package.
//!
//! The synthetic tests in `module_record_multi_942.rs` pin the semantics
//! (collisions, sibling exclusion). This one pins that it works on the actual
//! thing that motivated the issue — a real multi-module library whose head the
//! hub's resolver has never been able to read.
//!
//! Real-data tests are **opt-in**, because the fixture is a snapshot of prod
//! rather than something the repo can carry:
//!
//! ```bash
//! scratchpad/snapshot-prod-store.sh lex-official
//! LEX_PROD_FIXTURE=<scratchpad>/fixtures/lex-official \
//!   cargo test -p lex-store --test module_record_real_prod_942
//! ```
//!
//! Unset, every test here skips loudly rather than silently passing — so CI
//! stays green without the skip being invisible.

use lex_store::render::{module_record_at_op, module_record_at_op_for};
use lex_store::Store;
use lex_types::Ty;

/// The snapshot root (`.../lex-official`), or `None` with a loud note.
fn fixture() -> Option<std::path::PathBuf> {
    match std::env::var_os("LEX_PROD_FIXTURE") {
        Some(p) => {
            let p = std::path::PathBuf::from(p);
            assert!(
                p.join("stores").is_dir(),
                "LEX_PROD_FIXTURE must point at a tenant root containing `stores/`, got {}",
                p.display()
            );
            Some(p)
        }
        None => {
            eprintln!(
                "SKIP: set LEX_PROD_FIXTURE to a prod snapshot \
                 (scratchpad/snapshot-prod-store.sh lex-official) to run the real-data checks"
            );
            None
        }
    }
}

fn open_store(root: &std::path::Path, name: &str) -> Store {
    Store::open(root.join("stores").join(name)).expect("open snapshot store")
}

fn head_of(store: &Store) -> String {
    store
        .get_branch("main")
        .expect("get branch")
        .and_then(|b| b.head_op)
        .expect("main has a head")
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

/// `lex-schema` spans 21 files. Before #942 any module request on it failed as
/// `UnsupportedMultiModuleDependency`, so the hub's cross-store resolver could
/// not type-check a single dependent against the real library.
#[test]
fn a_real_21_file_library_resolves_a_named_module() {
    let Some(root) = fixture() else { return };
    let store = open_store(&root, "lex-schema");
    let head = head_of(&store);

    // `error` is one of the real modules (src/error.lex).
    let rec = module_record_at_op_for(&store, &head, Some("error"))
        .expect("a named module of a real multi-file library must resolve");
    let names = field_names(&rec);
    assert!(
        !names.is_empty(),
        "the module surface must not be empty — that would mean the filter \
         excluded everything rather than selecting one module"
    );
    // Sanity: this is one module's surface, not the whole 21-file package.
    assert!(
        names.len() < 100,
        "suspiciously large surface ({}) — looks like the whole package, not one module",
        names.len()
    );
    eprintln!("lex-schema/error surface: {} fn(s): {:?}", names.len(), names);
}

/// Two different modules of the same real head must yield different surfaces —
/// the property that proves scoping actually happened.
#[test]
fn two_modules_of_the_same_real_head_differ() {
    let Some(root) = fixture() else { return };
    let store = open_store(&root, "lex-schema");
    let head = head_of(&store);

    let a = module_record_at_op_for(&store, &head, Some("error")).expect("error module");
    let b = module_record_at_op_for(&store, &head, Some("coerce")).expect("coerce module");
    assert_ne!(
        field_names(&a),
        field_names(&b),
        "two modules of one package must not report the same surface"
    );
}

/// The pre-#942 entry point still refuses a multi-module head, rather than
/// silently returning one arbitrary module's surface as if it were the package's.
#[test]
fn the_unscoped_entry_point_still_refuses_a_multi_module_head() {
    let Some(root) = fixture() else { return };
    let store = open_store(&root, "lex-schema");
    let head = head_of(&store);

    assert!(
        module_record_at_op(&store, &head).is_err(),
        "without a module name a 21-file head must still be refused, not guessed at"
    );
}

/// A real single-file library keeps working through the unscoped entry point.
#[test]
fn a_real_single_file_library_still_resolves_unscoped() {
    let Some(root) = fixture() else { return };
    let store = open_store(&root, "lex-nt");
    let head = head_of(&store);

    let rec = module_record_at_op(&store, &head).expect("lex-nt is one file; must resolve");
    let names = field_names(&rec);
    assert!(names.contains(&"gcd".to_string()), "lex-nt exports gcd, got {names:?}");
}
