//! #566: `lex check` accumulates every independent error in one pass.
//!
//! Cross-function accumulation shipped in #582; these pin the within-body
//! "safe subset" — independent errors in discarded `Block` statements and
//! `Let` binding values are recovered so they all surface at once, without
//! manufacturing spurious follow-on errors in correct code.

use lex_ast::canonicalize_program;
use lex_syntax::parse_source;
use lex_types::check_program;

fn nerrs(src: &str) -> usize {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    match check_program(&stages) {
        Ok(_) => 0,
        Err(e) => e.len(),
    }
}

#[test]
fn let_chain_reports_both_independent_errors() {
    // Two `let` bindings whose values are both unknown identifiers. Before
    // #566's within-body recovery this reported only the first.
    let src = "fn c() -> Int {\n  let _x := nope_one()\n  let _y := nope_two()\n  0\n}\n";
    assert_eq!(nerrs(src), 2, "both bad let values should be reported");
}

#[test]
fn cross_function_still_accumulates() {
    // The #582 behaviour is preserved: one error per function, both reported.
    let src = "fn a() -> Int { \"x\" }\nfn b() -> Int { \"y\" }\n";
    assert_eq!(nerrs(src), 2);
}

#[test]
fn correct_multi_statement_body_has_no_spurious_errors() {
    // Recovery must not manufacture errors in correct code.
    let src = "fn ok() -> Int {\n  let a := 1\n  let b := 2\n  a\n}\n";
    assert_eq!(nerrs(src), 0);
}

#[test]
fn one_bad_let_then_valid_body_reports_one() {
    // A single bad value recovers to a fresh var; the rest of the body
    // checks cleanly, so exactly one error is reported (no cascade).
    let src = "fn d() -> Int {\n  let _x := nope_one()\n  0\n}\n";
    assert_eq!(nerrs(src), 1);
}
