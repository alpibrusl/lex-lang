//! Regression for #669: the builtin `SqlParam` constructors `PFloat`/`PBool`
//! must accept plain `Float`/`Bool` values. They were registered as the
//! nominal `Ty::Con("Float")` / `Ty::Con("Bool")` instead of the primitives
//! `Ty::float()` / `Ty::bool()`, so `PFloat(1.0)` failed to type-check with a
//! baffling "expected Float, got Float" (a `Con("Float")` that prints the same
//! as `Prim(Float)` but won't unify). `PStr`/`PInt` were unaffected because
//! they correctly used `Ty::str()` / `Ty::int()`.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::check_program;

fn check(src: &str) -> Result<(), Vec<lex_types::TypeError>> {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    check_program(&stages).map(|_| ())
}

#[test]
fn all_sqlparam_constructors_type_check() {
    // Every SqlParam constructor applied to a literal of its payload type,
    // plus the nullary PNull, must type-check.
    let src = r#"
fn p_str()   -> SqlParam { PStr("x") }
fn p_int()   -> SqlParam { PInt(1) }
fn p_float() -> SqlParam { PFloat(1.0) }
fn p_bool()  -> SqlParam { PBool(true) }
fn p_null()  -> SqlParam { PNull }
"#;
    check(src).unwrap_or_else(|errs| panic!("SqlParam constructors should type-check: {errs:#?}"));
}

#[test]
fn float_params_in_a_param_list() {
    // The shape that broke lex-truck-edge: a List[SqlParam] mixing PFloat/PInt/PStr.
    let src = r#"
fn params() -> List[SqlParam] {
  [PFloat(78.0), PInt(1), PStr("driving"), PBool(true)]
}
"#;
    check(src).unwrap_or_else(|errs| panic!("mixed SqlParam list should type-check: {errs:#?}"));
}
