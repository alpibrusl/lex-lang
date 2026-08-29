//! Integration tests for AES-CBC in `std.crypto` (#760): the
//! `aes_cbc_encrypt_raw` / `aes_cbc_decrypt_raw` interop pair.
//!
//! Unlike the AEAD tests next door, these DO pin ciphertext against a
//! published test vector. The AEAD tests can decline to, because a
//! round-trip through one implementation proves interoperability with
//! itself, and `aes-gcm` is separately vetted against NIST vectors.
//!
//! Here interoperating with a stranger's implementation is the entire
//! reason the primitive exists — a self-consistent AES-CBC that
//! disagrees with everyone else's would pass a round-trip test and fail
//! at the only job it has. So `nist_sp800_38a_f_2_1_vector` checks the
//! exact bytes from NIST SP 800-38A, and the round-trip tests check the
//! Lex surface around them.
//!
//! The other property under test is that nothing here can panic the VM:
//! wrong key length, wrong IV length, ragged ciphertext, and bad padding
//! all have to surface as `Err`.

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
                Value::Bytes(p) => p,
                other => panic!("expected Ok(Bytes), got Ok({other:?})"),
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
                other => panic!("expected Err(Str), got Err({other:?})"),
            }
        }
        other => panic!("expected Err(_), got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.crypto" as crypto

fn enc(key :: Bytes, iv :: Bytes, pt :: Bytes) -> Result[Bytes, Str] {
  crypto.aes_cbc_encrypt_raw(key, iv, pt)
}

fn dec(key :: Bytes, iv :: Bytes, ct :: Bytes) -> Result[Bytes, Str] {
  crypto.aes_cbc_decrypt_raw(key, iv, ct)
}
"#;

const KEY_128: [u8; 16] = [0u8; 16];
const KEY_192: [u8; 24] = [0u8; 24];
const KEY_256: [u8; 32] = [0u8; 32];
const IV: [u8; 16] = [0u8; 16];
const PLAINTEXT: &[u8] = b"the quick brown fox jumps over the lazy dog";

// ── interop: the property this primitive exists for ──────────────

/// NIST SP 800-38A, F.2.1 (CBC-AES128.Encrypt), first block.
///
/// This is the test that would actually catch a broken implementation.
/// A round-trip proves we agree with ourselves; this proves we agree
/// with everyone else, which is the only thing a caller speaking a
/// foreign wire format cares about.
#[test]
fn nist_sp800_38a_f_2_1_vector() {
    let key = hex(b"2b7e151628aed2a6abf7158809cf4f3c");
    let iv = hex(b"000102030405060708090a0b0c0d0e0f");
    let block = hex(b"6bc1bee22e409f96e93d7e117393172a");
    let expected = hex(b"7649abac8119b246cee98e9b12e9197d");

    let out = unwrap_ok_bytes(run(SRC, "enc", vec![b(&key), b(&iv), b(&block)]));
    assert_eq!(
        &out[..16],
        &expected[..],
        "first ciphertext block must match NIST SP 800-38A F.2.1"
    );
    // PKCS#7 pads a whole-block input with a full extra block, so a
    // 16-byte plaintext encrypts to 32 bytes. The vector covers the
    // first block; the second is our padding, not NIST's.
    assert_eq!(out.len(), 32, "whole-block input gains a full pad block");
}

fn hex(s: &[u8]) -> Vec<u8> {
    s.chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

// ── round trips, one per key length ──────────────────────────────

#[test]
fn round_trip_aes128() {
    let ct = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(PLAINTEXT)]));
    assert_eq!(ct.len() % 16, 0, "ciphertext is whole blocks");
    assert!(ct.len() > PLAINTEXT.len(), "PKCS#7 always adds padding");
    let pt = unwrap_ok_bytes(run(SRC, "dec", vec![b(&KEY_128), b(&IV), b(&ct)]));
    assert_eq!(pt, PLAINTEXT);
}

#[test]
fn round_trip_aes192_and_aes256() {
    for key in [&KEY_192[..], &KEY_256[..]] {
        let ct = unwrap_ok_bytes(run(SRC, "enc", vec![b(key), b(&IV), b(PLAINTEXT)]));
        let pt = unwrap_ok_bytes(run(SRC, "dec", vec![b(key), b(&IV), b(&ct)]));
        assert_eq!(pt, PLAINTEXT, "key length {} must round-trip", key.len());
    }
}

#[test]
fn empty_plaintext_round_trips_to_a_single_pad_block() {
    let ct = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(b"")]));
    assert_eq!(ct.len(), 16, "empty input is one block of pure padding");
    let pt = unwrap_ok_bytes(run(SRC, "dec", vec![b(&KEY_128), b(&IV), b(&ct)]));
    assert_eq!(pt, b"", "and decrypts back to nothing");
}

#[test]
fn key_length_selects_the_variant_rather_than_being_ignored() {
    // Same input, different key lengths, all-zero keys throughout: if the
    // key length were silently ignored (e.g. always AES-128), these would
    // collide. They must not.
    let a = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(PLAINTEXT)]));
    let c = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_256), b(&IV), b(PLAINTEXT)]));
    assert_ne!(a, c, "AES-128 and AES-256 must not produce the same ciphertext");
}

#[test]
fn the_iv_actually_reaches_the_cipher() {
    let a = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(PLAINTEXT)]));
    let other_iv = [9u8; 16];
    let c = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&other_iv), b(PLAINTEXT)]));
    assert_ne!(a, c, "a different IV must produce different ciphertext");
}

// ── every bad input is an Err, never a panic ─────────────────────

#[test]
fn wrong_key_length_is_an_error() {
    let e = unwrap_err(run(SRC, "enc", vec![b(&[0u8; 15]), b(&IV), b(PLAINTEXT)]));
    assert!(e.contains("key must be 16, 24, or 32 bytes"), "got: {e}");
}

#[test]
fn wrong_iv_length_is_an_error() {
    let e = unwrap_err(run(SRC, "enc", vec![b(&KEY_128), b(&[0u8; 12]), b(PLAINTEXT)]));
    assert!(e.contains("iv must be exactly 16 bytes"), "got: {e}");
}

#[test]
fn ciphertext_that_is_not_whole_blocks_is_an_error() {
    let e = unwrap_err(run(SRC, "dec", vec![b(&KEY_128), b(&IV), b(&[0u8; 17])]));
    assert!(e.contains("multiple of the 16-byte block size"), "got: {e}");
}

#[test]
fn empty_ciphertext_is_an_error_not_empty_plaintext() {
    // Distinct from the empty-PLAINTEXT case above: a valid empty message
    // encrypts to one pad block, so zero bytes cannot be a valid input.
    let e = unwrap_err(run(SRC, "dec", vec![b(&KEY_128), b(&IV), b(b"")]));
    assert!(e.contains("multiple of the 16-byte block size"), "got: {e}");
}

#[test]
fn the_wrong_key_is_rejected_by_padding_rather_than_returning_garbage() {
    let ct = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(PLAINTEXT)]));
    let e = unwrap_err(run(SRC, "dec", vec![b(&[7u8; 16]), b(&IV), b(&ct)]));
    assert!(e.contains("invalid padding or wrong key/iv"), "got: {e}");
}

#[test]
fn a_tampered_final_block_is_caught_by_the_padding_check() {
    // NOT a security claim. CBC is unauthenticated and this is exactly
    // why: tampering with any block BUT the last corrupts plaintext
    // silently, as the next test demonstrates. Pinned so the difference
    // between the two stays visible to whoever reads this file.
    let mut ct = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(PLAINTEXT)]));
    let last = ct.len() - 1;
    ct[last] ^= 0xff;
    let e = unwrap_err(run(SRC, "dec", vec![b(&KEY_128), b(&IV), b(&ct)]));
    assert!(e.contains("invalid padding or wrong key/iv"), "got: {e}");
}

#[test]
fn tampering_with_an_early_block_is_not_detected() {
    // The documented weakness, asserted rather than described. If this
    // test ever starts failing because someone "fixed" it by bolting a
    // MAC onto `_raw`, they have broken interop with every protocol this
    // primitive exists to speak. Add an authenticated mode instead.
    let mut ct = unwrap_ok_bytes(run(SRC, "enc", vec![b(&KEY_128), b(&IV), b(PLAINTEXT)]));
    ct[0] ^= 0x01;
    let pt = unwrap_ok_bytes(run(SRC, "dec", vec![b(&KEY_128), b(&IV), b(&ct)]));
    assert_ne!(pt, PLAINTEXT, "the plaintext is corrupted...");
    assert_eq!(pt.len(), PLAINTEXT.len(), "...and decryption still succeeds");
}

// ── the motivating case ──────────────────────────────────────────

#[test]
fn miio_key_derivation_composes_with_md5() {
    // The shape #760 was filed for: the miio protocol derives its AES
    // key and IV from an MD5 of the device token, both of which are now
    // expressible in Lex. Not a protocol test — just proof the pieces
    // compose without leaving the language.
    const SRC_MIIO: &str = r#"
import "std.crypto" as crypto

import "std.bytes" as bytes

fn miio_encrypt(token :: Bytes, payload :: Bytes) -> Result[Bytes, Str] {
  let key := crypto.md5(token)
  let iv := crypto.md5(bytes.concat(key, token))
  crypto.aes_cbc_encrypt_raw(key, iv, payload)
}
"#;
    let token = [0xABu8; 16];
    let payload = br#"{"id":1,"method":"miIO.info","params":[]}"#;
    let ct = unwrap_ok_bytes(run(SRC_MIIO, "miio_encrypt", vec![b(&token), b(payload)]));
    assert_eq!(ct.len() % 16, 0, "miio payloads are whole AES blocks");
    assert!(!ct.is_empty());
}
