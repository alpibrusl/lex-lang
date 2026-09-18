//! #930 phase 2b: `render::module_record_at_op` extracts a single-module
//! package head's public function signatures as a module record — the shape
//! the write-time gate hands to `check_program_with_modules` when it resolves
//! a dependency instead of inlining it.

use std::collections::BTreeMap;

use lex_ast::canonicalize_program;
use lex_store::render::module_record_at_op;
use lex_store::{Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_types::Ty;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn parse(src: &str) -> Vec<lex_ast::Stage> {
    canonicalize_program(&parse_source(src).expect("parse"))
}

/// Publish `src` from an empty base and return the new head op.
fn publish(store: &Store, src: &str) -> String {
    let stages = parse(src);
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
    store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &lex_vcs::ImportMap::new(), true)
        .expect("publish")
        .head_op
        .expect("head op")
}

#[test]
fn extracts_public_signatures_as_a_module_record() {
    let (store, _tmp) = fresh();
    let head = publish(
        &store,
        "fn gcd(a :: Int, b :: Int) -> Int { if b == 0 { a } else { gcd(b, a % b) } }\n\
         fn twice(n :: Int) -> Int { n + n }\n",
    );

    let rec = module_record_at_op(&store, &head).expect("record");
    let fields = match rec {
        Ty::Record(fs) => fs,
        other => panic!("expected a record, got {other:?}"),
    };

    // Both public functions are present as callable fields.
    assert!(fields.contains_key("gcd"), "gcd must be in the module record");
    assert!(fields.contains_key("twice"), "twice must be in the module record");

    // ...with their real signatures: gcd(Int, Int) -> Int.
    match &fields["gcd"] {
        Ty::Function { params, ret, .. } => {
            assert_eq!(params.len(), 2, "gcd takes two args");
            assert_eq!(params[0], Ty::int());
            assert_eq!(params[1], Ty::int());
            assert_eq!(**ret, Ty::int());
        }
        other => panic!("gcd should be a function, got {other:?}"),
    }
}

/// The extracted record is usable end to end: a dependent that imports this
/// package and calls `nt.gcd` type-checks against the resolved signatures,
/// without the dependency's bodies present.
#[test]
fn extracted_record_resolves_a_dependent() {
    let (store, _tmp) = fresh();
    let head = publish(
        &store,
        "fn gcd(a :: Int, b :: Int) -> Int { if b == 0 { a } else { gcd(b, a % b) } }\n",
    );
    let rec = module_record_at_op(&store, &head).expect("record");

    let mut modules = std::collections::BTreeMap::new();
    modules.insert("lex-nt/lib".to_string(), rec);

    let dependent = parse(
        "import \"lex-nt/lib\" as nt\nfn reduce(a :: Int, b :: Int) -> Int { nt.gcd(a, b) }\n",
    );
    lex_types::check_program_with_modules(&dependent, &modules)
        .unwrap_or_else(|e| panic!("dependent should check against the extracted record: {e:#?}"));

    // And the signature is enforced — wrong arg type still fails.
    let bad = parse(
        "import \"lex-nt/lib\" as nt\nfn reduce(a :: Str, b :: Int) -> Int { nt.gcd(a, b) }\n",
    );
    assert!(
        lex_types::check_program_with_modules(&bad, &modules).is_err(),
        "a Str argument to gcd(Int, Int) must still be rejected"
    );
}
