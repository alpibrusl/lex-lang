//! #930 completeness: a non-inlined dependency contributes its exported
//! *type* declarations, not just its function signatures. Before this,
//! `check_program_with_modules` carried only the dependency's value record, so
//! a package that referenced a dependency's exported type — in an annotation,
//! as an ADT constructor, or by constructing/reading its record — failed: the
//! type read as opaque, a matching record literal did not unify, and field
//! access on it errored. This surfaced migrating multi-module libraries
//! (lex-orm ↔ lex-schema's `ModelSchema`) to lex-official.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{
    check_program_with_module_ifaces, check_program_with_modules, module_record_from_fields,
    EffectSet, Ty,
};
use std::collections::BTreeMap;

fn stages(src: &str) -> Vec<lex_ast::Stage> {
    canonicalize_program(&parse_source(src).expect("parse"))
}

/// A dependency `dep/schema` exporting a record type `Rec` (referencing a
/// sibling type `Field`), an ADT `Kind`, and a fn `make() -> Rec`.
const DEP: &str = "\
type Field = { key :: Str }
type Rec = { title :: Str, fields :: List[Field] }
type Kind = KStr | KInt
fn make() -> Rec { { title: \"x\", fields: [] } }
";

fn dep_typedecls() -> Vec<lex_ast::TypeDecl> {
    stages(DEP)
        .into_iter()
        .filter_map(|s| match s {
            lex_ast::Stage::TypeDecl(td) => Some(td),
            _ => None,
        })
        .collect()
}

/// The dependency's value record — its fn signatures reference the dep's own
/// types by bare name, exactly as `module_record_at_op` / the client resolver
/// build it (`make() -> Rec`).
fn dep_values() -> Ty {
    module_record_from_fields(vec![(
        "make".to_string(),
        Ty::function(vec![], EffectSet::empty(), Ty::Con("Rec".to_string(), vec![])),
    )])
}

fn modules() -> BTreeMap<String, Ty> {
    let mut m = BTreeMap::new();
    m.insert("dep/schema".to_string(), dep_values());
    m
}
fn module_types() -> BTreeMap<String, Vec<lex_ast::TypeDecl>> {
    let mut m = BTreeMap::new();
    m.insert("dep/schema".to_string(), dep_typedecls());
    m
}

// A consumer that uses the dependency's TYPE three ways: an annotation
// (`-> s.Rec`), a record literal that must coerce to it, field access, an ADT
// constructor in a match, and a call whose return type is the dep type.
const CONSUMER: &str = "\
import \"dep/schema\" as s
fn build() -> s.Rec { { title: \"t\", fields: [] } }
fn title() -> Str {
  let r := build()
  r.title
}
fn from_make() -> Str { s.make().title }
fn kind_str(k :: s.Kind) -> Int { match k { KStr => 1, KInt => 2 } }
";

#[test]
fn dependency_types_resolve_when_supplied() {
    let s = stages(CONSUMER);
    check_program_with_module_ifaces(&s, &modules(), &module_types()).unwrap_or_else(|errs| {
        panic!("expected a clean check with the dependency's types supplied: {errs:#?}")
    });
}

#[test]
fn dependency_types_are_opaque_without_them() {
    // Negative control: the exact regression. With only the value record and
    // no type declarations, the consumer must NOT check — otherwise the test
    // above proves nothing.
    let s = stages(CONSUMER);
    assert!(
        check_program_with_modules(&s, &modules()).is_err(),
        "dep types must be unresolved when only the value record is supplied"
    );
    assert!(
        check_program_with_module_ifaces(&s, &modules(), &BTreeMap::new()).is_err(),
        "an empty module-types map must behave exactly like check_program_with_modules"
    );
}

#[test]
fn dependency_type_shape_is_actually_enforced() {
    // The dep record's shape is checked, not waved through: a literal missing a
    // required field (or with a wrong-typed one) annotated as `s.Rec` fails.
    let bad = "\
import \"dep/schema\" as s
fn build() -> s.Rec { { title: 42, fields: [] } }
";
    let s = stages(bad);
    assert!(
        check_program_with_module_ifaces(&s, &modules(), &module_types()).is_err(),
        "a record whose `title` is an Int must not satisfy s.Rec (title :: Str)"
    );
}

// ── #963: prefixed mode (transitive/within-package type identity) ────────────

use lex_types::check_program_with_deps;

// A dependency `dep` whose module `a` defines `Shared`, and whose module `b`
// re-exposes `Shared` in its own signature. Resolved as one package, both name
// the SAME canonical `a_h.Shared` (the diamond). The consumer imports both
// modules and also annotates with `sa.Shared`.
fn diamond_modules() -> BTreeMap<String, Ty> {
    let a = module_record_from_fields(vec![(
        "mk".into(),
        Ty::function(vec![], EffectSet::empty(), Ty::Con("a_h.Shared".into(), vec![])),
    )]);
    // `b.passthru()` returns the SAME `a_h.Shared` (b inlined a under a_h).
    let b = module_record_from_fields(vec![(
        "passthru".into(),
        Ty::function(vec![], EffectSet::empty(), Ty::Con("a_h.Shared".into(), vec![])),
    )]);
    BTreeMap::from([("dep/a".into(), a), ("dep/b".into(), b)])
}
fn diamond_types() -> BTreeMap<String, Vec<lex_ast::TypeDecl>> {
    // Prefix-named decl, as the whole-package resolver produces it: parse bare
    // (dots aren't legal in a source type name), then rename to the loader's
    // mangled form.
    let shared: Vec<lex_ast::TypeDecl> = stages("type Shared = { n :: Int }")
        .into_iter()
        .filter_map(|s| match s {
            lex_ast::Stage::TypeDecl(mut td) => {
                td.name = "a_h.Shared".to_string();
                Some(td)
            }
            _ => None,
        })
        .collect();
    // Both modules of the package carry the whole package's type decls.
    BTreeMap::from([("dep/a".into(), shared.clone()), ("dep/b".into(), shared)])
}
fn diamond_prefixes() -> BTreeMap<String, String> {
    BTreeMap::from([("dep/a".into(), "a_h".into()), ("dep/b".into(), "b_h".into())])
}

const DIAMOND_CONSUMER: &str = "\
import \"dep/a\" as sa
import \"dep/b\" as sb
fn width(x :: sa.Shared) -> Int { x.n }
fn from_b() -> Int { width(sb.passthru()) }
fn from_a() -> Int { width(sa.mk()) }
";

#[test]
fn prefixed_mode_unifies_a_type_reached_two_ways() {
    let s = stages(DIAMOND_CONSUMER);
    // `sa.Shared` (annotation) normalizes to `a_h.Shared`; `sb.passthru()` and
    // `sa.mk()` both return `a_h.Shared` — all one type.
    check_program_with_deps(&s, &diamond_modules(), &diamond_types(), &diamond_prefixes())
        .unwrap_or_else(|errs| panic!("diamond must unify under prefixed mode: {errs:#?}"));
}

#[test]
fn prefixed_mode_still_enforces_shape() {
    // `width` reads `.n :: Int`; calling it with an Int (not a Shared) fails.
    let bad = "\
import \"dep/a\" as sa
fn width(x :: sa.Shared) -> Int { x.n }
fn oops() -> Int { width(5) }
";
    let s = stages(bad);
    assert!(
        check_program_with_deps(&s, &diamond_modules(), &diamond_types(), &diamond_prefixes())
            .is_err(),
        "passing an Int where sa.Shared is expected must fail"
    );
}
