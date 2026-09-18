//! #930 phase 2b-3: the write-time gate consults the installed `DepResolver`
//! to type-check a head that keeps external `import` edges instead of inlining
//! the dependency. Without a resolver the same head is rejected (unbound
//! reference) — the pre-#930 behavior — so inlined heads stay unaffected.

use std::collections::BTreeMap;
use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_store::{DepResolver, Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_types::{module_record_from_fields, EffectSet, Ty};

/// A resolver that always supplies `lex-nt/lib` with `gcd(Int, Int) -> Int`.
struct NtResolver;
impl DepResolver for NtResolver {
    fn resolve_modules(&self, _stages: &[lex_ast::Stage], _head_op: Option<&str>) -> BTreeMap<String, Ty> {
        let rec = module_record_from_fields(vec![(
            "gcd".to_string(),
            Ty::function(vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int()),
        )]);
        let mut m = BTreeMap::new();
        m.insert("lex-nt/lib".to_string(), rec);
        m
    }
}

const HEAD: &str =
    "import \"lex-nt/lib\" as nt\nfn reduce(a :: Int, b :: Int) -> Int { nt.gcd(a, b) }\n";

fn stages() -> Vec<lex_ast::Stage> {
    canonicalize_program(&parse_source(HEAD).expect("parse"))
}

fn diff(stages: &[lex_ast::Stage]) -> lex_vcs::DiffReport {
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let empty = BTreeMap::new();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    lex_vcs::compute_diff_with_types(&empty, &new, &et, &et, true)
}

#[test]
fn head_with_external_edge_is_rejected_without_a_resolver() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let s = stages();
    let err = store
        .publish_program(DEFAULT_BRANCH, &s, &diff(&s), &lex_vcs::ImportMap::new(), true)
        .expect_err("nt.gcd must be unbound without a resolver");
    assert!(matches!(err, lex_store::StoreError::TypeError(_)));
    // Always-valid HEAD: the rejected publish left no branch.
    assert!(store.get_branch(DEFAULT_BRANCH).unwrap().is_none());
}

#[test]
fn head_with_external_edge_passes_with_a_resolver() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap().with_dep_resolver(Arc::new(NtResolver));
    let s = stages();
    let outcome = store
        .publish_program(DEFAULT_BRANCH, &s, &diff(&s), &lex_vcs::ImportMap::new(), true)
        .expect("the resolver supplies nt.gcd's signature, so the head type-checks");
    assert!(outcome.head_op.is_some(), "the branch advanced");
}
