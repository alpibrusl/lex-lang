//! Underscore-prefixed identifiers and `_` discard in `let` (#200).
//!
//! Soft hit this in their v0.2.0 build report: stub host
//! programs in soft-agent's tests use Rust's `_name` convention
//! to mark "intentionally exists but unused," and the parser
//! rejected the form. The fix in #200 accepts `_name` as a
//! regular identifier and `_` as a discard binding in let.

use lex_syntax::parse_source;

fn assert_parses(src: &str) {
    parse_source(src).unwrap_or_else(|e| panic!("expected to parse, got {e}\n--- src ---\n{src}"));
}

#[test]
fn fn_name_can_start_with_underscore() {
    assert_parses("fn _host(x :: Int) -> Int { x + 1 }\n");
}

#[test]
fn fn_param_can_start_with_underscore() {
    assert_parses("fn id(_unused :: Int, y :: Int) -> Int { y }\n");
}

#[test]
fn let_binding_name_can_start_with_underscore() {
    assert_parses("fn main() -> Int { let _seen := 1\n_seen }\n");
}

#[test]
fn let_underscore_discards_value() {
    // The classic "evaluate for effect, ignore the value"
    // pattern. The RHS is type-checked and run; the result
    // simply isn't bound to a name user code can reach.
    assert_parses("fn main() -> Int { let _ := 1 + 2\n42 }\n");
}

#[test]
fn multiple_let_underscore_in_sequence() {
    // Each discard gets a unique synthetic name, so a sequence
    // of them doesn't collide. (Pre-#200 fix this would have
    // been a parse error on the first `let _`.)
    assert_parses("fn main() -> Int {\n  let _ := 1\n  let _ := 2\n  let _ := 3\n  42\n}\n");
}

#[test]
fn underscore_alone_is_not_an_identifier() {
    // The bare `_` is reserved for the discard role: in match
    // arms (existing behaviour) and now in let. Using it as
    // a *name* should fail — not silently lex as Ident.
    let r = parse_source("fn _(x :: Int) -> Int { x }\n");
    assert!(r.is_err(), "bare `_` is not a valid fn name");
}

#[test]
fn double_underscore_identifier() {
    // Edge case: `__foo` and `__` should both work as
    // identifiers (the regex's underscore-prefix branch matches
    // any `_x` where x is alphanumeric or another underscore).
    assert_parses("fn __helper(x :: Int) -> Int { x }\n");
    assert_parses("fn user(__opaque :: Int) -> Int { __opaque + 1 }\n");
}

#[test]
fn underscore_match_arm_pattern_still_works() {
    // Pin the existing discard semantics in match arms — the
    // #200 fix shouldn't have changed it.
    assert_parses("fn classify(n :: Int) -> Int {\n  match n {\n    0 => 0,\n    _ => 1,\n  }\n}\n");
}
