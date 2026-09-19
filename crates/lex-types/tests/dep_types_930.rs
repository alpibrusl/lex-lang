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
