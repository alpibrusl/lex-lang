//! Integration tests for AEAD primitives in `std.crypto` (#382 AEAD
//! slice): `aes_gcm_seal/open` and `chacha20_poly1305_seal/open`.
//!
//! The tests focus on three correctness properties:
//! 1. Round-trip: `open(seal(pt)) == pt`.
//! 2. Authentication: a flipped bit in ciphertext, tag, key, nonce, or
//!    aad makes `open` return Err.
//! 3. Input validation: wrong key/nonce/tag length surfaces as Err
//!    (not a runtime panic).
//!
//! We don't pin specific ciphertext values against NIST/RFC test
//! vectors here — the underlying `aes-gcm` and `chacha20poly1305`
//! crates are themselves vetted against those vectors. Our concern is
//! that the Lex surface plumbing is correct end-to-end.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

/// Build a Lex `Bytes` value from a Rust byte slice.
fn b(xs: &[u8]) -> Value { Value::Bytes(xs.to_vec()) }

/// Unwrap an `Ok(...)` variant; panic on `Err`.
fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => args.into_iter().next().unwrap(),
        other => panic!("expected Ok(_), got {other:?}"),
    }
}

/// Check that a value is an `Err(_)` variant; return the inner Str.
fn unwrap_err(v: Value) -> String {
    match v {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => match args.into_iter().next().unwrap() {
            Value::Str(s) => s.to_string(),
            other => panic!("Err payload not Str: {other:?}"),
        },
        other => panic!("expected Err(_), got {other:?}"),
    }
}

/// Unpack an AeadResult `{ ciphertext, tag }` record into Rust bytes.
fn unwrap_aead_result(v: Value) -> (Vec<u8>, Vec<u8>) {
    let rec = match v {
        Value::Record { fields: r, .. } => r,
        other => panic!("expected AeadResult record, got {other:?}"),
    };
    let ct = match rec.get("ciphertext") {
        Some(Value::Bytes(b)) => b.clone(),
        other => panic!("AeadResult.ciphertext: expected Bytes, got {other:?}"),
    };
    let tag = match rec.get("tag") {
        Some(Value::Bytes(b)) => b.clone(),
        other => panic!("AeadResult.tag: expected Bytes, got {other:?}"),
    };
    (ct, tag)
}

const SRC: &str = r#"
import "std.crypto" as crypto

# AES-GCM round-trip wrappers
fn aes_seal(key :: Bytes, nonce :: Bytes, aad :: Bytes, pt :: Bytes) -> Result[AeadResult, Str] {
  crypto.aes_gcm_seal(key, nonce, aad, pt)
}
fn aes_open(key :: Bytes, nonce :: Bytes, aad :: Bytes, ct :: Bytes, tag :: Bytes) -> Result[Bytes, Str] {
  crypto.aes_gcm_open(key, nonce, aad, ct, tag)
}

# ChaCha20-Poly1305 round-trip wrappers
fn cc_seal(key :: Bytes, nonce :: Bytes, aad :: Bytes, pt :: Bytes) -> Result[AeadResult, Str] {
  crypto.chacha20_poly1305_seal(key, nonce, aad, pt)
}
fn cc_open(key :: Bytes, nonce :: Bytes, aad :: Bytes, ct :: Bytes, tag :: Bytes) -> Result[Bytes, Str] {
  crypto.chacha20_poly1305_open(key, nonce, aad, ct, tag)
}
"#;

// Fixed test material. Real callers must source `key` and `nonce` from
// `crypto.random` — these literals are only for deterministic tests.
const AES128_KEY: [u8; 16] = [0u8; 16];
const AES256_KEY: [u8; 32] = [0u8; 32];
const CHACHA_KEY: [u8; 32] = [0u8; 32];
const NONCE_12: [u8; 12] = [0u8; 12];
const PLAINTEXT: &[u8] = b"the quick brown fox jumps over the lazy dog";
const AAD: &[u8] = b"v1:metadata=foo";

// ── AES-GCM ──────────────────────────────────────────────────────

#[test]
fn aes_gcm_128_round_trip() {
    let r = run(
        SRC, "aes_seal",
        vec![b(&AES128_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (ct, tag) = unwrap_aead_result(unwrap_ok(r));
    assert_eq!(ct.len(), PLAINTEXT.len(), "AES-GCM ciphertext = plaintext length");
    assert_eq!(tag.len(), 16, "AES-GCM tag is 16 bytes");

    let pt = run(
        SRC, "aes_open",
        vec![b(&AES128_KEY), b(&NONCE_12), b(AAD), b(&ct), b(&tag)],
    );
    let recovered = match unwrap_ok(pt) {
        Value::Bytes(p) => p,
        other => panic!("expected Bytes, got {other:?}"),
    };
    assert_eq!(recovered, PLAINTEXT, "round-trip must recover plaintext");
}

#[test]
fn aes_gcm_256_round_trip() {
    let r = run(
        SRC, "aes_seal",
        vec![b(&AES256_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (ct, tag) = unwrap_aead_result(unwrap_ok(r));
    let pt = run(
        SRC, "aes_open",
        vec![b(&AES256_KEY), b(&NONCE_12), b(AAD), b(&ct), b(&tag)],
    );
    let recovered = match unwrap_ok(pt) {
        Value::Bytes(p) => p,
        other => panic!("expected Bytes, got {other:?}"),
    };
    assert_eq!(recovered, PLAINTEXT);
}

#[test]
fn aes_gcm_rejects_modified_ciphertext() {
    let r = run(
        SRC, "aes_seal",
        vec![b(&AES128_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (mut ct, tag) = unwrap_aead_result(unwrap_ok(r));
    ct[0] ^= 1; // flip a single bit
    let pt = run(
        SRC, "aes_open",
        vec![b(&AES128_KEY), b(&NONCE_12), b(AAD), b(&ct), b(&tag)],
    );
    let _ = unwrap_err(pt); // any Err is acceptable; we just require not Ok
}

#[test]
fn aes_gcm_rejects_modified_aad() {
    let r = run(
        SRC, "aes_seal",
        vec![b(&AES128_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (ct, tag) = unwrap_aead_result(unwrap_ok(r));
    let bad_aad = b"v2:different-aad";
    let pt = run(
        SRC, "aes_open",
        vec![b(&AES128_KEY), b(&NONCE_12), b(bad_aad), b(&ct), b(&tag)],
    );
    let _ = unwrap_err(pt);
}

#[test]
fn aes_gcm_rejects_bad_key_length() {
    let bad_key = [0u8; 20]; // not 16 or 32
    let r = run(
        SRC, "aes_seal",
        vec![b(&bad_key), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let msg = unwrap_err(r);
    assert!(msg.contains("16 or 32"), "expected key-length error message: {msg}");
}

#[test]
fn aes_gcm_rejects_bad_nonce_length() {
    let short_nonce = [0u8; 8];
    let r = run(
        SRC, "aes_seal",
        vec![b(&AES128_KEY), b(&short_nonce), b(AAD), b(PLAINTEXT)],
    );
    let msg = unwrap_err(r);
    assert!(msg.contains("12 bytes"), "expected nonce-length error message: {msg}");
}

// ── ChaCha20-Poly1305 ────────────────────────────────────────────

#[test]
fn chacha20_round_trip() {
    let r = run(
        SRC, "cc_seal",
        vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (ct, tag) = unwrap_aead_result(unwrap_ok(r));
    assert_eq!(ct.len(), PLAINTEXT.len());
    assert_eq!(tag.len(), 16);

    let pt = run(
        SRC, "cc_open",
        vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(&ct), b(&tag)],
    );
    let recovered = match unwrap_ok(pt) {
        Value::Bytes(p) => p,
        other => panic!("expected Bytes, got {other:?}"),
    };
    assert_eq!(recovered, PLAINTEXT);
}

#[test]
fn chacha20_rejects_modified_tag() {
    let r = run(
        SRC, "cc_seal",
        vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (ct, mut tag) = unwrap_aead_result(unwrap_ok(r));
    tag[0] ^= 1;
    let pt = run(
        SRC, "cc_open",
        vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(&ct), b(&tag)],
    );
    let _ = unwrap_err(pt);
}

#[test]
fn chacha20_rejects_wrong_key() {
    let r = run(
        SRC, "cc_seal",
        vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let (ct, tag) = unwrap_aead_result(unwrap_ok(r));
    let other_key = [0xffu8; 32];
    let pt = run(
        SRC, "cc_open",
        vec![b(&other_key), b(&NONCE_12), b(AAD), b(&ct), b(&tag)],
    );
    let _ = unwrap_err(pt);
}

#[test]
fn chacha20_rejects_bad_key_length() {
    let bad_key = [0u8; 16]; // ChaCha20-Poly1305 only takes 32
    let r = run(
        SRC, "cc_seal",
        vec![b(&bad_key), b(&NONCE_12), b(AAD), b(PLAINTEXT)],
    );
    let msg = unwrap_err(r);
    assert!(msg.contains("32 bytes"), "expected key-length error: {msg}");
}

#[test]
fn aead_empty_plaintext_is_handled() {
    // Edge case: zero-byte plaintext. The cipher should still produce
    // a 16-byte tag that authenticates the (empty) plaintext + aad.
    let r = run(SRC, "cc_seal", vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(&[])]);
    let (ct, tag) = unwrap_aead_result(unwrap_ok(r));
    assert!(ct.is_empty(), "ciphertext of empty plaintext must be empty");
    assert_eq!(tag.len(), 16);

    let pt = run(SRC, "cc_open", vec![b(&CHACHA_KEY), b(&NONCE_12), b(AAD), b(&ct), b(&tag)]);
    let recovered = match unwrap_ok(pt) {
        Value::Bytes(p) => p,
        other => panic!("expected Bytes, got {other:?}"),
    };
    assert!(recovered.is_empty());
}
