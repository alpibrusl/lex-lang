//! #930 keystone: the checker can resolve a registry/git dependency from a
//! caller-supplied module map instead of requiring the dependency to be
//! inlined into the program. This is the precondition for the loader to
//! stop inlining deps (a later phase) — with these signatures reachable, a
//! head that keeps `import "<pkg>/mod" as <alias>` edges and references
//! `<alias>.name` still type-checks against the resolved dependency.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::{
    check_program, check_program_with_modules, module_record_from_fields, EffectSet, Ty, TypeError,
};
use std::collections::BTreeMap;

fn stages(src: &str) -> Vec<lex_ast::Stage> {
    let p = parse_source(src).expect("parse");
    canonicalize_program(&p)
}

/// A resolved `lex-nt/lib` exporting `gcd(Int, Int) -> Int`.
fn lex_nt_modules() -> BTreeMap<String, Ty> {
    let lib = module_record_from_fields(vec![(
        "gcd".to_string(),
        Ty::function(vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int()),
    )]);
    let mut m = BTreeMap::new();
    m.insert("lex-nt/lib".to_string(), lib);
    m
}

const USES_DEP: &str =
    "import \"lex-nt/lib\" as nt\nfn run(a :: Int, b :: Int) -> Int { nt.gcd(a, b) }\n";

#[test]
fn dependency_reference_checks_when_module_supplied() {
    let s = stages(USES_DEP);
    check_program_with_modules(&s, &lex_nt_modules())
        .unwrap_or_else(|errs| panic!("expected clean check with the dep supplied: {errs:#?}"));
}

#[test]
fn dependency_reference_is_unbound_without_the_module() {
    // Negative control: the same source, checked with no supplied modules
    // (exactly what `check_program` does today), must NOT pass — otherwise
    // the test above would prove nothing. `nt` is bound by neither stdlib
    // nor the (empty) dependency map.
    let s = stages(USES_DEP);
    assert!(
        check_program(&s).is_err(),
        "nt.gcd must be unresolved when the dependency isn't supplied"
    );
    assert!(
        check_program_with_modules(&s, &BTreeMap::new()).is_err(),
        "empty module map must behave exactly like check_program"
    );
}

#[test]
fn supplied_dependency_signature_is_actually_enforced() {
    // The dep's signature is checked, not waved through: passing a Str where
    // `gcd` wants an Int is still a type error even with the module supplied.
    let src =
        "import \"lex-nt/lib\" as nt\nfn run(a :: Str, b :: Int) -> Int { nt.gcd(a, b) }\n";
    let s = stages(src);
    let errs = check_program_with_modules(&s, &lex_nt_modules())
        .map(|_| ())
        .expect_err("Str argument to gcd(Int, Int) must be rejected");
    assert!(!errs.is_empty());
}

/// The ret-type var id of a `field: (..) -> ret` function type, for probing
/// what `module_record_from_fields` numbered each export.
fn ret_var(field: &Ty) -> u32 {
    match field {
        Ty::Function { ret, .. } => match ret.as_ref() {
            Ty::Var(v) => *v,
            other => panic!("expected a var return, got {other:?}"),
        },
        other => panic!("expected a function type, got {other:?}"),
    }
}

fn param0_var(field: &Ty) -> u32 {
    match field {
        Ty::Function { params, .. } => match &params[0] {
            Ty::Var(v) => *v,
            other => panic!("expected a var param, got {other:?}"),
        },
        other => panic!("expected a function type, got {other:?}"),
    }
}

#[test]
fn distinct_exports_get_disjoint_type_vars() {
    // Two polymorphic exports, each authored with Var(0) — as they would be
    // coming from two independently generalized dependency signatures. After
    // assembly they must NOT share a type variable, or generalizing the
    // record as a whole would tie them together.
    let id_a = Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(0));
    let id_b = Ty::function(vec![Ty::Var(0)], EffectSet::empty(), Ty::Var(0));
    let rec = module_record_from_fields(vec![("a".to_string(), id_a), ("b".to_string(), id_b)]);
    let fs = match rec {
        Ty::Record(fs) => fs,
        other => panic!("expected a record, got {other:?}"),
    };
    let a = &fs["a"];
    let b = &fs["b"];
    // Within each export the variable stays one variable (param == ret)...
    assert_eq!(param0_var(a), ret_var(a), "export a must keep its var consistent");
    assert_eq!(param0_var(b), ret_var(b), "export b must keep its var consistent");
    // ...but the two exports must use different ids.
    assert_ne!(ret_var(a), ret_var(b), "distinct exports must get disjoint type vars");
}

#[test]
fn monomorphic_exports_pass_through_unchanged() {
    let gcd = Ty::function(vec![Ty::int(), Ty::int()], EffectSet::empty(), Ty::int());
    let rec = module_record_from_fields(vec![("gcd".to_string(), gcd.clone())]);
    match rec {
        Ty::Record(fs) => assert_eq!(fs["gcd"], gcd, "a monomorphic export must be untouched"),
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn empty_module_map_preserves_stdlib_behaviour() {
    // A stdlib-only program checks identically through the new entry point
    // with an empty map — the non-breaking guarantee for the 7 existing
    // `check_program` gate call sites.
    let src = "import \"std.int\" as int\nfn label(n :: Int) -> Str { int.to_str(n) }\n";
    let s = stages(src);
    let via_plain = check_program(&s).map(|_| ()).map_err(|e: Vec<TypeError>| e.len());
    let via_modules = check_program_with_modules(&s, &BTreeMap::new())
        .map(|_| ())
        .map_err(|e: Vec<TypeError>| e.len());
    assert_eq!(via_plain, via_modules);
    via_plain.expect("stdlib-only program checks clean");
}
