//! Integration tests for KDF primitives in `std.crypto` (#382 KDF
//! slice): `pbkdf2_sha256`, `hkdf_sha256`, `argon2id`.
//!
//! Coverage:
//! 1. Known-answer vectors against the RFC test vectors so we catch
//!    accidental algorithm swaps (e.g. PBKDF2-SHA1 vs SHA256).
//! 2. Determinism: identical inputs produce identical outputs.
//! 3. Output length matches the requested `len` argument.
//! 4. Input validation: bad iterations / len / argon2 cost surface as
//!    `Err`, not a VM panic.
//!
//! The underlying `pbkdf2`, `hkdf`, and `argon2` crates are themselves
//! vetted against the upstream RFC vectors; our concern is that the
//! Lex surface plumbing is correct end-to-end.

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

fn b(xs: &[u8]) -> Value { Value::Bytes(xs.to_vec()) }

fn unwrap_ok_bytes(v: Value) -> Vec<u8> {
    match v {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => {
            match args.into_iter().next().unwrap() {
                Value::Bytes(bs) => bs,
                other => panic!("Ok payload not Bytes: {other:?}"),
            }
        }
        other => panic!("expected Ok(_), got {other:?}"),
    }
}

fn unwrap_err(v: Value) -> String {
    match v {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => {
            match args.into_iter().next().unwrap() {
                Value::Str(s) => s.to_string(),
                other => panic!("Err payload not Str: {other:?}"),
            }
        }
        other => panic!("expected Err(_), got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.crypto" as crypto

fn pb(password :: Bytes, salt :: Bytes, iters :: Int, len :: Int) -> Result[Bytes, Str] {
  crypto.pbkdf2_sha256(password, salt, iters, len)
}

fn hk(ikm :: Bytes, salt :: Bytes, info :: Bytes, len :: Int) -> Result[Bytes, Str] {
  crypto.hkdf_sha256(ikm, salt, info, len)
}

fn ar(password :: Bytes, salt :: Bytes, t :: Int, m :: Int, len :: Int) -> Result[Bytes, Str] {
  crypto.argon2id(password, salt, t, m, len)
}
"#;

// ── PBKDF2-SHA256 ──────────────────────────────────────────────────────────

#[test]
fn pbkdf2_sha256_rfc7914_test_vector() {
    // From RFC 7914 §11 (Appendix B in some editions): PBKDF2-HMAC-SHA256
    // with password="passwd", salt="salt", c=1, dkLen=64 yields:
    //   55ac046e56e3089fec1691c22544b605
    //   f94185216dde0465e68b9d57c20dacbc
    //   49ca9cccf179b645991664b39d77ef31
    //   7c71b845b1e30bd509112041d3a19783
    let out = unwrap_ok_bytes(run(SRC, "pb", vec![
        b(b"passwd"), b(b"salt"), Value::Int(1), Value::Int(64),
    ]));
    let expected = hex::decode(
        "55ac046e56e3089fec1691c22544b605\
         f94185216dde0465e68b9d57c20dacbc\
         49ca9cccf179b645991664b39d77ef31\
         7c71b845b1e30bd509112041d3a19783"
    ).unwrap();
    assert_eq!(out, expected, "PBKDF2-SHA256 RFC 7914 vector mismatch");
}

#[test]
fn pbkdf2_sha256_is_deterministic() {
    let a = unwrap_ok_bytes(run(SRC, "pb", vec![
        b(b"hunter2"), b(b"sodium-chloride"), Value::Int(1000), Value::Int(32),
    ]));
    let b_out = unwrap_ok_bytes(run(SRC, "pb", vec![
        b(b"hunter2"), b(b"sodium-chloride"), Value::Int(1000), Value::Int(32),
    ]));
    assert_eq!(a, b_out);
}

#[test]
fn pbkdf2_sha256_different_password_different_output() {
    let a = unwrap_ok_bytes(run(SRC, "pb", vec![
        b(b"hunter2"), b(b"salt"), Value::Int(100), Value::Int(32),
    ]));
    let b_out = unwrap_ok_bytes(run(SRC, "pb", vec![
        b(b"hunter3"), b(b"salt"), Value::Int(100), Value::Int(32),
    ]));
    assert_ne!(a, b_out);
}

#[test]
fn pbkdf2_sha256_honours_len() {
    let out = unwrap_ok_bytes(run(SRC, "pb", vec![
        b(b"x"), b(b"y"), Value::Int(2), Value::Int(48),
    ]));
    assert_eq!(out.len(), 48);
}

#[test]
fn pbkdf2_sha256_rejects_zero_iterations() {
    let err = unwrap_err(run(SRC, "pb", vec![
        b(b"x"), b(b"y"), Value::Int(0), Value::Int(32),
    ]));
    assert!(err.contains("iterations"), "got: {err}");
}

#[test]
fn pbkdf2_sha256_rejects_zero_len() {
    let err = unwrap_err(run(SRC, "pb", vec![
        b(b"x"), b(b"y"), Value::Int(1), Value::Int(0),
    ]));
    assert!(err.contains("len"), "got: {err}");
}

// ── HKDF-SHA256 ────────────────────────────────────────────────────────────

#[test]
fn hkdf_sha256_rfc5869_test_case_1() {
    // RFC 5869 §A.1 Test Case 1:
    //   IKM  = 0x0b * 22
    //   salt = 0x000102030405060708090a0b0c
    //   info = 0xf0f1f2f3f4f5f6f7f8f9
    //   L    = 42
    // OKM = 3cb25f25faacd57a90434f64d0362f2a
    //       2d2d0a90cf1a5a4c5db02d56ecc4c5bf
    //       34007208d5b887185865
    let out = unwrap_ok_bytes(run(SRC, "hk", vec![
        b(&[0x0b; 22]),
        b(&hex::decode("000102030405060708090a0b0c").unwrap()),
        b(&hex::decode("f0f1f2f3f4f5f6f7f8f9").unwrap()),
        Value::Int(42),
    ]));
    let expected = hex::decode(
        "3cb25f25faacd57a90434f64d0362f2a\
         2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
         34007208d5b887185865"
    ).unwrap();
    assert_eq!(out, expected, "HKDF-SHA256 RFC 5869 case 1 mismatch");
}

#[test]
fn hkdf_sha256_empty_salt_uses_default() {
    // Empty salt is allowed (RFC 5869 substitutes a zero-string of
    // HashLen). We just check that the call succeeds; the well-known
    // vector for empty-salt is covered by the hkdf crate's tests.
    let out = unwrap_ok_bytes(run(SRC, "hk", vec![
        b(&[0x0b; 22]), b(b""), b(b""), Value::Int(42),
    ]));
    assert_eq!(out.len(), 42);
}

#[test]
fn hkdf_sha256_rejects_oversized_output() {
    // HKDF-SHA256 caps output at 255 * 32 = 8160 bytes. 9000 must Err.
    // (The exact message varies with the hkdf crate version — assert
    // we got an Err, not a specific phrasing.)
    let err = unwrap_err(run(SRC, "hk", vec![
        b(b"ikm"), b(b"salt"), b(b"info"), Value::Int(9000),
    ]));
    assert!(err.starts_with("hkdf_sha256:"), "got: {err}");
}

// ── Argon2id ───────────────────────────────────────────────────────────────

#[test]
fn argon2id_is_deterministic() {
    // argon2 is intentionally slow; keep the cost parameters tiny so
    // the test suite stays snappy. The point here is the round-trip,
    // not the work factor — `lex-crypto`'s wrapper will pin a
    // production-grade default.
    let a = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    let b_out = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    assert_eq!(a, b_out, "argon2id must be deterministic");
    assert_eq!(a.len(), 32);
}

#[test]
fn argon2id_password_change_changes_output() {
    let a = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    let b_out = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter3"), b(b"salty-salt"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    assert_ne!(a, b_out);
}

#[test]
fn argon2id_salt_change_changes_output() {
    // Argon2 specifies a minimum salt length of 8 bytes.
    let a = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salt-A-padded-out"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    let b_out = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salt-B-padded-out"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    assert_ne!(a, b_out);
}

#[test]
fn argon2id_rejects_short_salt() {
    // Argon2 minimum salt length is 8 bytes; surface that as Err.
    let err = unwrap_err(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"short"),
        Value::Int(1), Value::Int(8), Value::Int(32),
    ]));
    assert!(err.to_lowercase().contains("salt"), "got: {err}");
}

#[test]
fn argon2id_rejects_zero_t_cost() {
    let err = unwrap_err(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(0), Value::Int(8), Value::Int(32),
    ]));
    assert!(err.contains("t_cost"), "got: {err}");
}

#[test]
fn argon2id_rejects_too_small_m_cost() {
    // m_cost must be >= MIN_M_COST (8); 1 must Err.
    let err = unwrap_err(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(1), Value::Int(1), Value::Int(32),
    ]));
    assert!(err.contains("m_cost"), "got: {err}");
}

#[test]
fn argon2id_rejects_zero_len() {
    let err = unwrap_err(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(1), Value::Int(8), Value::Int(0),
    ]));
    assert!(err.contains("len"), "got: {err}");
}

#[test]
fn argon2id_honours_len() {
    let out = unwrap_ok_bytes(run(SRC, "ar", vec![
        b(b"hunter2"), b(b"salty-salt"),
        Value::Int(1), Value::Int(8), Value::Int(64),
    ]));
    assert_eq!(out.len(), 64);
}
