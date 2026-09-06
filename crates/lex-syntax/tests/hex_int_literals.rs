//! Hex (`0x1F`) and binary (`0b1010`) integer literals.
//!
//! Before this, `0x80` lexed as `Int(0)` immediately followed by
//! `Ident("x80")` (the identifier regex happily matches `x80`) --
//! silently wrong rather than a lex error, so a call like
//! `bytes.singleton(0x80)` failed downstream in the parser with a
//! confusing "expected RParen, got Ident" instead of naming the real
//! problem. Reproduced live: an agent writing RLP byte-encoding (where
//! `0x80`-style constants are the natural idiom) hit exactly this.

use lex_syntax::token::{lex, TokenKind};

fn int_tokens(src: &str) -> Vec<i64> {
    lex(src)
        .unwrap_or_else(|e| panic!("expected to lex, got {e:?}\n--- src ---\n{src}"))
        .into_iter()
        .filter_map(|t| match t.kind {
            TokenKind::Int(n) => Some(n),
            _ => None,
        })
        .collect()
}

#[test]
fn hex_literal_lexes_as_single_int() {
    assert_eq!(int_tokens("0x80"), vec![128]);
}

#[test]
fn hex_literal_uppercase_prefix_and_digits() {
    assert_eq!(int_tokens("0XFF"), vec![255]);
}

#[test]
fn hex_literal_with_underscores() {
    assert_eq!(int_tokens("0xDE_AD_BE_EF"), vec![0xDEADBEEF]);
}

#[test]
fn binary_literal_lexes_as_single_int() {
    assert_eq!(int_tokens("0b1010"), vec![10]);
}

#[test]
fn plain_zero_is_unaffected() {
    assert_eq!(int_tokens("0"), vec![0]);
}

#[test]
fn decimal_still_wins_when_no_hex_prefix() {
    assert_eq!(int_tokens("0 + 128"), vec![0, 128]);
}

#[test]
fn hex_literal_parses_in_a_real_call() {
    // The exact failure mode this fixes: previously `bytes.u8(0x80)`
    // parsed as a two-argument call `bytes.u8(0, x80)` — the fix here
    // is what lets this be a real integration point, not just a lexer
    // test, but the crate boundary keeps this test at the lexer level.
    let toks = lex("bytes.u8(0x80)").expect("should lex");
    let ints: Vec<i64> = toks
        .iter()
        .filter_map(|t| match &t.kind {
            TokenKind::Int(n) => Some(*n),
            _ => None,
        })
        .collect();
    assert_eq!(ints, vec![128]);
    // No stray identifier from a split "0" + "x80".
    assert!(!toks.iter().any(|t| matches!(&t.kind, TokenKind::Ident(s) if s == "x80")));
}
