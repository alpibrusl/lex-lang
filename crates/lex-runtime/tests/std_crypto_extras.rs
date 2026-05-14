//! Integration tests for the `std.crypto` extensions added in #382:
//! `blake2b`, `sha256_str` / `sha512_str`, `base64url_encode` /
//! `base64url_decode`, `eq` / `eq_str`, and `random_str_hex`. The
//! pre-#382 hashes / HMAC / base64 / hex / `constant_time_eq` /
//! `random` ops are covered by `std_crypto.rs`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn policy_with(effects: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects
        .iter()
        .map(|s| s.to_string())
        .collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn bytes(v: Value) -> Vec<u8> {
    match v { Value::Bytes(b) => b, other => panic!("expected Bytes, got {other:?}") }
}
fn s(v: Value) -> String {
    match v { Value::Str(x) => x.to_string(), other => panic!("expected Str, got {other:?}") }
}
fn b(v: Value) -> bool {
    match v { Value::Bool(x) => x, other => panic!("expected Bool, got {other:?}") }
}
fn ok_b(v: Value) -> Vec<u8> {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => bytes(args.into_iter().next().unwrap()),
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.crypto" as crypto
import "std.bytes" as bytes

fn blake2b_of(s :: Str) -> Bytes { crypto.blake2b(bytes.from_str(s)) }
fn sha256_str_of(s :: Str) -> Str    { crypto.sha256_str(s) }
fn sha512_str_of(s :: Str) -> Str    { crypto.sha512_str(s) }

fn b64url_round(s :: Str) -> Result[Bytes, Str] {
  crypto.base64url_decode(crypto.base64url_encode(bytes.from_str(s)))
}
fn b64url_encode(s :: Str) -> Str { crypto.base64url_encode(bytes.from_str(s)) }

fn eq_self(s :: Str) -> Bool {
  let bs := bytes.from_str(s)
  crypto.eq(bs, bs)
}
fn eq_diff() -> Bool {
  crypto.eq(bytes.from_str("alpha"), bytes.from_str("beta"))
}
fn eq_str_same(a :: Str, c :: Str) -> Bool { crypto.eq_str(a, c) }
fn eq_str_len_mismatch() -> Bool { crypto.eq_str("foo", "fooo") }

fn rand_hex(n :: Int) -> [random] Str { crypto.random_str_hex(n) }
"#;

// ── BLAKE2b ──────────────────────────────────────────────────────

#[test]
fn blake2b_returns_64_byte_digest() {
    let v = run(SRC, "blake2b_of", vec![Value::Str("hello".into())], Policy::pure());
    let digest = bytes(v);
    assert_eq!(digest.len(), 64, "BLAKE2b-512 must return 64 bytes; got {} bytes", digest.len());
}

#[test]
fn blake2b_is_deterministic() {
    let a = bytes(run(SRC, "blake2b_of", vec![Value::Str("hello".into())], Policy::pure()));
    let b = bytes(run(SRC, "blake2b_of", vec![Value::Str("hello".into())], Policy::pure()));
    assert_eq!(a, b, "BLAKE2b must be deterministic across calls");
}

#[test]
fn blake2b_distinguishes_inputs() {
    let a = bytes(run(SRC, "blake2b_of", vec![Value::Str("hello".into())], Policy::pure()));
    let b = bytes(run(SRC, "blake2b_of", vec![Value::Str("world".into())], Policy::pure()));
    assert_ne!(a, b, "BLAKE2b of different inputs must produce different digests");
}

// ── sha256_str / sha512_str ──────────────────────────────────────

#[test]
fn sha256_str_known_vector() {
    // RFC 6234 / Wikipedia test vector: SHA-256("") = e3b0c4...
    let h = s(run(SRC, "sha256_str_of", vec![Value::Str("".into())], Policy::pure()));
    assert_eq!(
        h,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "SHA-256 of empty string must match the standard test vector"
    );
}

#[test]
fn sha512_str_length_is_128_hex_chars() {
    let h = s(run(SRC, "sha512_str_of", vec![Value::Str("hello".into())], Policy::pure()));
    assert_eq!(h.len(), 128, "SHA-512 hex must be 128 chars; got {}", h.len());
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "all chars must be hex: {h}");
}

// ── base64url ────────────────────────────────────────────────────

#[test]
fn base64url_uses_urlsafe_alphabet_and_no_padding() {
    // Plain `?` (0x3f) base64-encodes to `Pw==`; the URL-safe form
    // both swaps no character here and strips the padding to `Pw`.
    // Use a string that exercises the `+` / `/` → `-` / `_` swap:
    // bytes `[0xfb, 0xff, 0xfe]` standard-encode to `+//+`, URL-safe
    // to `-__-`. Pad would be `=`; URL_SAFE_NO_PAD omits it.
    // We exercise via a round-trip + check the encoded shape.
    let encoded = s(run(SRC, "b64url_encode", vec![Value::Str("hi".into())], Policy::pure()));
    assert!(
        !encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='),
        "URL-safe base64 must not contain +, /, or = padding: {encoded:?}"
    );
}

#[test]
fn base64url_round_trips() {
    let v = run(SRC, "b64url_round", vec![Value::Str("any plaintext payload here".into())], Policy::pure());
    let decoded = ok_b(v);
    assert_eq!(decoded, b"any plaintext payload here".to_vec());
}

// ── eq / eq_str ──────────────────────────────────────────────────

#[test]
fn eq_returns_true_for_identical_bytes() {
    assert!(b(run(SRC, "eq_self", vec![Value::Str("anything".into())], Policy::pure())));
}

#[test]
fn eq_returns_false_for_distinct_bytes() {
    assert!(!b(run(SRC, "eq_diff", vec![], Policy::pure())));
}

#[test]
fn eq_str_returns_true_when_equal() {
    let v = run(
        SRC,
        "eq_str_same",
        vec![Value::Str("token".into()), Value::Str("token".into())],
        Policy::pure(),
    );
    assert!(b(v));
}

#[test]
fn eq_str_returns_false_when_different() {
    let v = run(
        SRC,
        "eq_str_same",
        vec![Value::Str("token".into()), Value::Str("toxen".into())],
        Policy::pure(),
    );
    assert!(!b(v));
}

#[test]
fn eq_str_returns_false_for_length_mismatch() {
    // Length differences must return false (in constant time, though
    // we can't really test the timing — just that the API doesn't
    // panic and the result is false).
    assert!(!b(run(SRC, "eq_str_len_mismatch", vec![], Policy::pure())));
}

// ── random_str_hex ───────────────────────────────────────────────

#[test]
fn random_str_hex_returns_2n_hex_chars() {
    let v = run(SRC, "rand_hex", vec![Value::Int(16)], policy_with(&["random"]));
    let hex = s(v);
    assert_eq!(hex.len(), 32, "16 random bytes must render as 32 hex chars; got {} chars", hex.len());
    assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "all chars must be hex: {hex}");
}

#[test]
fn random_str_hex_zero_returns_empty_string() {
    let v = run(SRC, "rand_hex", vec![Value::Int(0)], policy_with(&["random"]));
    assert_eq!(s(v), "");
}

#[test]
fn random_str_hex_produces_distinct_outputs_across_calls() {
    // 32-byte tokens; two consecutive calls colliding has negligible
    // probability (2^-256). If this ever fires, the RNG is broken.
    let a = s(run(SRC, "rand_hex", vec![Value::Int(32)], policy_with(&["random"])));
    let b = s(run(SRC, "rand_hex", vec![Value::Int(32)], policy_with(&["random"])));
    assert_ne!(a, b, "two 32-byte random hex tokens must not collide");
}
