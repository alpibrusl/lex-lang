//! #930: the hub-CI gate (`verify_head_and_attest`) reconstructs a head's
//! `import` edges before type-checking, so a non-inlined head whose deps are
//! resolvable passes — not `unknown_identifier`. Regression for the bug found
//! in production: the head's AddImport ops aren't in the SigId→stage map, so
//! the gate saw no `import` to bind the alias to and failed even though the
//! dependency resolved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_store::{DepResolver, Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_types::{module_record_from_fields, EffectSet, Ty};
use lex_vcs::{ImportMap, ImportRef};

/// Supplies `lex-nt/lib` with `gcd(Int, Int) -> Int`.
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

#[test]
fn hub_verify_resolves_a_non_inlined_dep_head() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap().with_dep_resolver(Arc::new(NtResolver));

    let src = "import \"lex-nt/lib\" as nt\nfn reduce(a :: Int, b :: Int) -> Int { nt.gcd(a, b) }\n";
    let stages = canonicalize_program(&parse_source(src).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&BTreeMap::new(), &new, &et, &et, true);

    // Record the package import as an AddImport edge (what a non-inlined
    // publish does), so the op-log head carries it.
    let mut imports: ImportMap = BTreeMap::new();
    let mut set = BTreeSet::new();
    set.insert(ImportRef { reference: "lex-nt/lib".to_string(), alias: "nt".to_string() });
    imports.insert("src/main.lex".to_string(), set);

    let head = store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &imports, true)
        .expect("publish (client gate resolves via the resolver)")
        .head_op
        .expect("head op");

    // The hub-CI gate reconstructs the import edge from the op-log and resolves
    // `nt.gcd` — a passing verdict, not `unknown_identifier "nt"`.
    let verdict = store
        .verify_head_and_attest(DEFAULT_BRANCH, None, &head)
        .expect("verify");
    assert!(
        verdict.passed,
        "hub verify must resolve the non-inlined dep, got: {:?}",
        verdict.detail
    );
}
