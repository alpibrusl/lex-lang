//! #946: reconstructing a head for replay must include its `import` edges.
//!
//! `program_stages_at_op` rebuilds a program from the SigId→stage map, which
//! holds only fn/type declarations — a head's imports are `AddImport` ops and
//! are absent from it. For a non-inlined head (#930 made that the default) the
//! reconstruction therefore produced a program whose `<alias>.name` calls had
//! no import to bind against, and `replay_request` handed a regenerator parent
//! source that silently omitted the very dependencies the code calls into.
//!
//! That is worse than useless as context: a model asked to regenerate
//! `reduce` would see it calling `nt.gcd` with no `nt` in sight.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lex_ast::canonicalize_program;
use lex_store::{DepResolver, Store, DEFAULT_BRANCH};
use lex_syntax::parse_source;
use lex_types::{module_record_from_fields, EffectSet, Ty};
use lex_vcs::{ImportMap, ImportRef};

struct NtResolver;
impl DepResolver for NtResolver {
    fn resolve_modules(
        &self,
        _stages: &[lex_ast::Stage],
        _head_op: Option<&str>,
    ) -> BTreeMap<String, Ty> {
        let rec = module_record_from_fields(vec![(
            "gcd".to_string(),
            Ty::function(vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int()),
        )]);
        let mut m = BTreeMap::new();
        m.insert("lex-nt/lib".to_string(), rec);
        m
    }
}

const SRC: &str =
    "import \"lex-nt/lib\" as nt\nfn reduce(a :: Int, b :: Int) -> Int { nt.gcd(a, b) }\n";

/// Publish a non-inlined head that imports `lex-nt/lib`; return its head op.
fn non_inlined_head() -> (Store, tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap().with_dep_resolver(Arc::new(NtResolver));

    let stages = canonicalize_program(&parse_source(SRC).expect("parse"));
    let new: BTreeMap<String, lex_ast::FnDecl> = stages
        .iter()
        .filter_map(|st| match st {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let et: BTreeMap<String, lex_ast::TypeDecl> = BTreeMap::new();
    let diff = lex_vcs::compute_diff_with_types(&BTreeMap::new(), &new, &et, &et, true);

    let mut imports: ImportMap = BTreeMap::new();
    let mut set = BTreeSet::new();
    set.insert(ImportRef { reference: "lex-nt/lib".to_string(), alias: "nt".to_string() });
    imports.insert("src/main.lex".to_string(), set);

    let head = store
        .publish_program(DEFAULT_BRANCH, &stages, &diff, &imports, true)
        .expect("publish")
        .head_op
        .expect("head op");
    (store, tmp, head)
}

#[test]
fn reconstructing_a_non_inlined_head_keeps_its_imports() {
    let (store, _tmp, head) = non_inlined_head();
    let stages = store.program_stages_at_op(&head).expect("reconstruct");

    let imports: Vec<_> = stages
        .iter()
        .filter_map(|s| match s {
            lex_ast::Stage::Import(i) => Some((i.reference.as_str(), i.alias.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        imports,
        vec![("lex-nt/lib", "nt")],
        "the reconstructed head must carry its import edge: {stages:?}"
    );

    // And the fn is still there — the imports are added, not substituted.
    assert!(
        stages.iter().any(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == "reduce")),
        "the reconstruction must still contain the declarations"
    );
}

#[test]
fn the_rendered_parent_program_shows_the_import() {
    let (store, _tmp, head) = non_inlined_head();
    let src = lex_ast::print_stages(&store.program_stages_at_op(&head).expect("reconstruct"));
    assert!(
        src.contains("lex-nt/lib") && src.contains("nt"),
        "parent source handed to a regenerator must name the dependency it calls into:\n{src}"
    );
}

/// An inlined (dependency-free) head is unaffected — no imports to add, and
/// nothing spurious appears.
#[test]
fn a_head_with_no_imports_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let stages = canonicalize_program(
        &parse_source("fn double(n :: Int) -> Int { n * 2 }\n").expect("parse"),
    );
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
        .expect("head op");

    let got = store.program_stages_at_op(&head).expect("reconstruct");
    assert!(
        !got.iter().any(|s| matches!(s, lex_ast::Stage::Import(_))),
        "a head with no imports must gain none: {got:?}"
    );
    assert_eq!(got.len(), 1);
}
