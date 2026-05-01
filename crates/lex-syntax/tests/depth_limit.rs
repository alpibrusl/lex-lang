//! Regression tests for the parser's recursion-depth gate.
//!
//! Found by libFuzzer: deeply-nested `[`/`{` blew the stack.
//! The fix caps `parse_expr` recursion at `MAX_DEPTH` and returns
//! a clean ParseError instead of unwinding.

use lex_syntax::parse_source;

#[test]
fn deeply_nested_lists_yield_clean_error_not_stack_overflow() {
    // 1000 nested `[`s — well past the 256 cap, well past whatever
    // a stack might tolerate. Pre-fix this would SIGSEGV.
    let src = "fn f() -> Int { ".to_string()
        + &"[".repeat(1000)
        + " 1 "
        + &"]".repeat(1000)
        + " }";
    let err = parse_source(&src).unwrap_err().to_string();
    assert!(err.contains("nests too deeply"),
        "expected depth error, got: {err}");
}

#[test]
fn deeply_nested_records_yield_clean_error() {
    // The original libFuzzer crash input mixed `[` and `{` —
    // verify the gate catches braces too.
    let src = "fn f() -> Int { ".to_string()
        + &"{ x: ".repeat(1000)
        + "1"
        + &" }".repeat(1000)
        + " }";
    let err = parse_source(&src).unwrap_err().to_string();
    assert!(err.contains("nests too deeply"),
        "expected depth error, got: {err}");
}

#[test]
fn modestly_nested_input_still_parses() {
    // 50 nested lists is well under the cap; legitimate code can
    // still nest expressions deeply.
    let src = "fn f() -> Int { ".to_string()
        + &"[".repeat(50)
        + " 1 "
        + &"]".repeat(50)
        + " }";
    parse_source(&src).expect("50-deep input must parse");
}
