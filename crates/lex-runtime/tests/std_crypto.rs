//! Integration tests for `std.crypto`. Closes #102.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn run_with_policy(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
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

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    run_with_policy(src, fn_name, args, Policy::pure())
}

fn bytes(v: Value) -> Vec<u8> {
    match v {
        Value::Bytes(b) => b,
        other => panic!("expected Bytes, got {other:?}"),
    }
}

fn s(v: Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        other => panic!("expected Str, got {other:?}"),
    }
}

const HASH_SRC: &str = r#"
import "std.crypto" as crypto
fn h_sha256(x :: Bytes) -> Bytes { crypto.sha256(x) }
fn h_sha512(x :: Bytes) -> Bytes { crypto.sha512(x) }
fn h_md5(x :: Bytes) -> Bytes { crypto.md5(x) }
fn h_sha256_hex(x :: Bytes) -> Str { crypto.hex_encode(crypto.sha256(x)) }
"#;

#[test]
fn sha256_known_vector() {
    // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
    let v = run(HASH_SRC, "h_sha256_hex", vec![Value::Bytes(b"hello".to_vec())]);
    assert_eq!(
        s(v),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn sha256_empty_input() {
    let v = run(HASH_SRC, "h_sha256", vec![Value::Bytes(vec![])]);
    let b = bytes(v);
    assert_eq!(b.len(), 32);
    // SHA-256("") known prefix
    assert_eq!(b[0], 0xe3);
    assert_eq!(b[1], 0xb0);
}

#[test]
fn sha512_length() {
    let v = run(HASH_SRC, "h_sha512", vec![Value::Bytes(b"hello".to_vec())]);
    assert_eq!(bytes(v).len(), 64);
}

#[test]
fn md5_length() {
    let v = run(HASH_SRC, "h_md5", vec![Value::Bytes(b"hello".to_vec())]);
    assert_eq!(bytes(v).len(), 16);
}

const HMAC_SRC: &str = r#"
import "std.crypto" as crypto
fn mac256(key :: Bytes, data :: Bytes) -> Str {
  crypto.hex_encode(crypto.hmac_sha256(key, data))
}
"#;

#[test]
fn hmac_sha256_known_vector() {
    // RFC 4231 test 1:
    //   key  = 0b * 20
    //   data = "Hi There"
    //   mac  = b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
    let key = vec![0x0b; 20];
    let v = run(
        HMAC_SRC,
        "mac256",
        vec![Value::Bytes(key), Value::Bytes(b"Hi There".to_vec())],
    );
    assert_eq!(
        s(v),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
}

const ENCODE_SRC: &str = r#"
import "std.crypto" as crypto
fn b64(x :: Bytes) -> Str { crypto.base64_encode(x) }
fn unb64(s :: Str) -> Result[Bytes, Str] { crypto.base64_decode(s) }
fn h(x :: Bytes) -> Str { crypto.hex_encode(x) }
fn unh(s :: Str) -> Result[Bytes, Str] { crypto.hex_decode(s) }
"#;

#[test]
fn base64_round_trip() {
    let input = b"hello, world!".to_vec();
    let encoded = run(ENCODE_SRC, "b64", vec![Value::Bytes(input.clone())]);
    assert_eq!(s(encoded.clone()), "aGVsbG8sIHdvcmxkIQ==");
    let decoded = run(ENCODE_SRC, "unb64", vec![encoded]);
    match decoded {
        Value::Variant { name, args } => {
            assert_eq!(name, "Ok");
            match &args[0] {
                Value::Bytes(b) => assert_eq!(b, &input),
                other => panic!("expected Bytes inside Ok, got {other:?}"),
            }
        }
        other => panic!("expected Result variant, got {other:?}"),
    }
}

#[test]
fn base64_decode_invalid() {
    let v = run(ENCODE_SRC, "unb64", vec![Value::Str("not!@#valid".into())]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err"),
        other => panic!("expected Err variant, got {other:?}"),
    }
}

#[test]
fn hex_round_trip() {
    let input = vec![0xde, 0xad, 0xbe, 0xef];
    let encoded = run(ENCODE_SRC, "h", vec![Value::Bytes(input.clone())]);
    assert_eq!(s(encoded.clone()), "deadbeef");
    let decoded = run(ENCODE_SRC, "unh", vec![encoded]);
    if let Value::Variant { name, args } = decoded {
        assert_eq!(name, "Ok");
        if let Value::Bytes(b) = &args[0] {
            assert_eq!(b, &input);
            return;
        }
    }
    panic!("hex round-trip failed");
}

#[test]
fn hex_decode_invalid_length() {
    let v = run(ENCODE_SRC, "unh", vec![Value::Str("abc".into())]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err"),
        other => panic!("expected Err variant, got {other:?}"),
    }
}

const CTEQ_SRC: &str = r#"
import "std.crypto" as crypto
fn cmp(a :: Bytes, b :: Bytes) -> Bool { crypto.constant_time_eq(a, b) }
"#;

#[test]
fn constant_time_eq_match() {
    let v = run(
        CTEQ_SRC,
        "cmp",
        vec![Value::Bytes(b"abcd".to_vec()), Value::Bytes(b"abcd".to_vec())],
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn constant_time_eq_mismatch() {
    let v = run(
        CTEQ_SRC,
        "cmp",
        vec![Value::Bytes(b"abcd".to_vec()), Value::Bytes(b"abce".to_vec())],
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn constant_time_eq_length_mismatch_returns_false() {
    let v = run(
        CTEQ_SRC,
        "cmp",
        vec![Value::Bytes(b"abc".to_vec()), Value::Bytes(b"abcd".to_vec())],
    );
    assert_eq!(v, Value::Bool(false));
}

const RANDOM_SRC: &str = r#"
import "std.crypto" as crypto
fn make_token(n :: Int) -> [random] Bytes { crypto.random(n) }
"#;

fn random_policy() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["random".to_string()].into_iter().collect::<BTreeSet<_>>();
    p
}

#[test]
fn random_returns_requested_length() {
    let v = run_with_policy(
        RANDOM_SRC,
        "make_token",
        vec![Value::Int(32)],
        random_policy(),
    );
    assert_eq!(bytes(v).len(), 32);
}

#[test]
fn random_zero_length_is_allowed() {
    let v = run_with_policy(
        RANDOM_SRC,
        "make_token",
        vec![Value::Int(0)],
        random_policy(),
    );
    assert_eq!(bytes(v).len(), 0);
}

#[test]
fn random_two_calls_differ() {
    let a = bytes(run_with_policy(
        RANDOM_SRC,
        "make_token",
        vec![Value::Int(32)],
        random_policy(),
    ));
    let b = bytes(run_with_policy(
        RANDOM_SRC,
        "make_token",
        vec![Value::Int(32)],
        random_policy(),
    ));
    // 1 in 2^256 chance of false failure; effectively impossible.
    assert_ne!(a, b);
}

#[test]
fn random_without_grant_is_rejected_by_static_policy_walk() {
    use lex_runtime::policy::check_program;
    let prog = parse_source(RANDOM_SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("type-check");
    let bc = compile_program(&stages);
    let policy = Policy::pure(); // no [random] grant
    let result = check_program(&bc, &policy);
    assert!(
        result.is_err(),
        "expected static policy walk to reject [random] without grant"
    );
}

// ─── ed25519 asymmetric signatures (#643) ────────────────────────────────────
const ED25519_SRC: &str = r#"
import "std.crypto" as crypto

fn roundtrip(secret :: Bytes, msg :: Bytes) -> Bool {
  match crypto.ed25519_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.ed25519_sign(secret, msg) {
      Err(_) => false,
      Ok(sig) => crypto.ed25519_verify(pk, msg, sig),
    },
  }
}
fn wrong_msg(secret :: Bytes, msg :: Bytes, other :: Bytes) -> Bool {
  match crypto.ed25519_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.ed25519_sign(secret, msg) {
      Err(_) => false,
      Ok(sig) => crypto.ed25519_verify(pk, other, sig),
    },
  }
}
fn pub_of(secret :: Bytes) -> Result[Bytes, Str] { crypto.ed25519_public_key(secret) }
"#;

fn is_true(v: Value) -> bool {
    match v {
        Value::Bool(b) => b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn ed25519_sign_verify_roundtrip() {
    let secret = vec![7u8; 32];
    let msg = b"transfer authorized".to_vec();
    let ok = run(ED25519_SRC, "roundtrip",
        vec![Value::Bytes(secret.clone()), Value::Bytes(msg.clone())]);
    assert!(is_true(ok), "valid signature must verify");
}

#[test]
fn ed25519_rejects_wrong_message() {
    let secret = vec![7u8; 32];
    let msg = b"transfer 100".to_vec();
    let other = b"transfer 9999".to_vec();
    let bad = run(ED25519_SRC, "wrong_msg",
        vec![Value::Bytes(secret), Value::Bytes(msg), Value::Bytes(other)]);
    assert!(!is_true(bad), "signature must not verify for a different message");
}

#[test]
fn ed25519_public_key_is_deterministic_and_32_bytes() {
    let secret = vec![3u8; 32];
    let a = run(ED25519_SRC, "pub_of", vec![Value::Bytes(secret.clone())]);
    let b = run(ED25519_SRC, "pub_of", vec![Value::Bytes(secret)]);
    match (a, b) {
        (Value::Variant { name: n1, args: a1 }, Value::Variant { name: n2, args: a2 }) => {
            assert_eq!(n1, "Ok");
            assert_eq!(n2, "Ok");
            match (&a1[0], &a2[0]) {
                (Value::Bytes(p1), Value::Bytes(p2)) => {
                    assert_eq!(p1.len(), 32, "public key must be 32 bytes");
                    assert_eq!(p1, p2, "derivation must be deterministic");
                }
                other => panic!("expected Bytes, got {other:?}"),
            }
        }
        other => panic!("expected Ok variants, got {other:?}"),
    }
}

#[test]
fn ed25519_public_key_rejects_bad_length() {
    let v = run(ED25519_SRC, "pub_of", vec![Value::Bytes(vec![1u8; 10])]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err", "10-byte secret must Err"),
        other => panic!("expected Result, got {other:?}"),
    }
}

// ─── P-256 ECDSA / ES256 (#651) ──────────────────────────────────────────────
const P256_SRC: &str = r#"
import "std.crypto" as crypto

# generate -> sign -> verify round-trip, returns Ok(true) on success.
fn mint() -> [random] Result[Bytes, Str] { crypto.p256_generate() }

fn pub_of(secret :: Bytes) -> Result[Bytes, Str] { crypto.p256_public_key(secret) }

fn roundtrip(secret :: Bytes, msg :: Bytes) -> Bool {
  match crypto.p256_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.p256_sign(secret, msg) {
      Err(_) => false,
      Ok(sig) => crypto.p256_verify(pk, msg, sig),
    },
  }
}
fn wrong_msg(secret :: Bytes, msg :: Bytes, other :: Bytes) -> Bool {
  match crypto.p256_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.p256_sign(secret, msg) {
      Err(_) => false,
      Ok(sig) => crypto.p256_verify(pk, other, sig),
    },
  }
}
fn sign_of(secret :: Bytes, msg :: Bytes) -> Result[Bytes, Str] {
  crypto.p256_sign(secret, msg)
}
"#;

// A fixed, valid 32-byte P-256 scalar (NIST test vector private key).
fn p256_test_secret() -> Vec<u8> {
    // 0xC9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721
    hex::decode("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721")
        .expect("valid hex")
}

#[test]
fn p256_generate_returns_32_byte_secret() {
    let v = run_with_policy(P256_SRC, "mint", vec![], random_policy());
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Bytes(b) => assert_eq!(b.len(), 32, "P-256 secret scalar is 32 bytes"),
            other => panic!("expected Bytes, got {other:?}"),
        },
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

#[test]
fn p256_generate_two_calls_differ() {
    let mint = || match run_with_policy(P256_SRC, "mint", vec![], random_policy()) {
        Value::Variant { name, args } if name == "Ok" => bytes(args.into_iter().next().unwrap()),
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_ne!(mint(), mint(), "fresh keys must differ");
}

#[test]
fn p256_public_key_is_33_byte_compressed_point() {
    let v = run(P256_SRC, "pub_of", vec![Value::Bytes(p256_test_secret())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Bytes(b) => {
                assert_eq!(b.len(), 33, "SEC1 compressed point is 33 bytes");
                // Compressed points start with 0x02 or 0x03.
                assert!(b[0] == 0x02 || b[0] == 0x03, "leading byte tags y-parity");
            }
            other => panic!("expected Bytes, got {other:?}"),
        },
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

#[test]
fn p256_sign_verify_roundtrip() {
    let secret = p256_test_secret();
    let msg = b"checkout mandate: pay 42.00 USD".to_vec();
    let ok = run(P256_SRC, "roundtrip",
        vec![Value::Bytes(secret), Value::Bytes(msg)]);
    assert!(is_true(ok), "valid ES256 signature must verify");
}

#[test]
fn p256_rejects_wrong_message() {
    let secret = p256_test_secret();
    let msg = b"transfer 100".to_vec();
    let other = b"transfer 9999".to_vec();
    let bad = run(P256_SRC, "wrong_msg",
        vec![Value::Bytes(secret), Value::Bytes(msg), Value::Bytes(other)]);
    assert!(!is_true(bad), "signature must not verify for a different message");
}

#[test]
fn p256_signature_is_der_encoded() {
    let v = run(P256_SRC, "sign_of",
        vec![Value::Bytes(p256_test_secret()), Value::Bytes(b"hi".to_vec())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            // DER SEQUENCE of two INTEGERs: leading tag 0x30, and a
            // P-256 signature DER-encodes to roughly 70-72 bytes.
            Value::Bytes(b) => {
                assert_eq!(b[0], 0x30, "DER signature starts with SEQUENCE tag");
                assert!((68..=72).contains(&b.len()), "P-256 DER sig is ~70 bytes, got {}", b.len());
            }
            other => panic!("expected Bytes, got {other:?}"),
        },
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

#[test]
fn p256_public_key_rejects_bad_length() {
    let v = run(P256_SRC, "pub_of", vec![Value::Bytes(vec![1u8; 10])]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err", "10-byte secret must Err"),
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn p256_generate_without_grant_is_rejected_by_static_policy_walk() {
    use lex_runtime::policy::check_program;
    let prog = parse_source(P256_SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("type-check");
    let bc = compile_program(&stages);
    let policy = Policy::pure(); // no [random] grant
    assert!(
        check_program(&bc, &policy).is_err(),
        "expected static policy walk to reject p256_generate's [random] without grant"
    );
}

// ─── secp256k1 / keccak256 — EVM (EIP-712 / x402) primitives (#655) ───────────
const SECP_SRC: &str = r#"
import "std.crypto" as crypto

fn mint() -> [random] Result[Bytes, Str] { crypto.secp256k1_generate() }

fn pub_of(secret :: Bytes) -> Result[Bytes, Str] { crypto.secp256k1_public_key(secret) }

fn keccak(data :: Bytes) -> Bytes { crypto.keccak256(data) }

fn sign_of(secret :: Bytes, digest :: Bytes) -> Result[Bytes, Str] {
  crypto.secp256k1_sign_digest(secret, digest)
}

# sign a digest, recover the pubkey, check it matches the signer's pubkey.
fn recover_matches(secret :: Bytes, digest :: Bytes) -> Bool {
  match crypto.secp256k1_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.secp256k1_sign_digest(secret, digest) {
      Err(_) => false,
      Ok(sig) => match crypto.secp256k1_recover(digest, sig) {
        Err(_) => false,
        Ok(rec) => crypto.eq(pk, rec),
      },
    },
  }
}

fn verify_roundtrip(secret :: Bytes, digest :: Bytes) -> Bool {
  match crypto.secp256k1_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.secp256k1_sign_digest(secret, digest) {
      Err(_) => false,
      Ok(sig) => crypto.secp256k1_verify(pk, digest, sig),
    },
  }
}

fn verify_wrong_digest(secret :: Bytes, digest :: Bytes, other :: Bytes) -> Bool {
  match crypto.secp256k1_public_key(secret) {
    Err(_) => false,
    Ok(pk) => match crypto.secp256k1_sign_digest(secret, digest) {
      Err(_) => false,
      Ok(sig) => crypto.secp256k1_verify(pk, other, sig),
    },
  }
}
"#;

// A fixed, valid 32-byte secp256k1 scalar (the well-known "1" test key's
// sibling — any in-range scalar works; this is deterministic for the tests).
fn secp_test_secret() -> Vec<u8> {
    hex::decode("4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318")
        .expect("valid hex")
}

// A fixed 32-byte digest (stands in for an EIP-712 signing digest).
fn secp_test_digest() -> Vec<u8> {
    // keccak256("") — a stable 32-byte value; the signer doesn't re-hash.
    hex::decode("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
        .expect("valid hex")
}

#[test]
fn keccak256_empty_matches_known_vector() {
    // The canonical Keccak-256 of the empty input (Ethereum's hash).
    let v = run(SECP_SRC, "keccak", vec![Value::Bytes(vec![])]);
    assert_eq!(
        hex::encode(bytes(v)),
        "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
        "keccak256(\"\") must match the Ethereum constant (not SHA3-256)"
    );
}

#[test]
fn keccak256_is_not_sha3_256() {
    // SHA3-256("") is a000...80a5; Keccak-256 differs in padding. Guard
    // against accidentally wiring up Sha3_256 instead of Keccak256.
    let v = run(SECP_SRC, "keccak", vec![Value::Bytes(vec![])]);
    assert_ne!(
        hex::encode(bytes(v)),
        "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a",
        "keccak256 must not equal SHA3-256"
    );
}

#[test]
fn secp256k1_generate_returns_32_byte_secret() {
    let v = run_with_policy(SECP_SRC, "mint", vec![], random_policy());
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Bytes(b) => assert_eq!(b.len(), 32, "secp256k1 secret scalar is 32 bytes"),
            other => panic!("expected Bytes, got {other:?}"),
        },
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

#[test]
fn secp256k1_generate_two_calls_differ() {
    let mint = || match run_with_policy(SECP_SRC, "mint", vec![], random_policy()) {
        Value::Variant { name, args } if name == "Ok" => bytes(args.into_iter().next().unwrap()),
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_ne!(mint(), mint(), "fresh keys must differ");
}

#[test]
fn secp256k1_public_key_is_65_byte_uncompressed_point() {
    let v = run(SECP_SRC, "pub_of", vec![Value::Bytes(secp_test_secret())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Bytes(b) => {
                assert_eq!(b.len(), 65, "uncompressed SEC1 point is 65 bytes");
                assert_eq!(b[0], 0x04, "uncompressed points start with 0x04");
            }
            other => panic!("expected Bytes, got {other:?}"),
        },
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

#[test]
fn secp256k1_signature_is_65_bytes_with_eth_v() {
    let v = run(SECP_SRC, "sign_of",
        vec![Value::Bytes(secp_test_secret()), Value::Bytes(secp_test_digest())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Bytes(b) => {
                assert_eq!(b.len(), 65, "r‖s‖v is 65 bytes");
                assert!(b[64] == 27 || b[64] == 28, "Ethereum v is 27 or 28, got {}", b[64]);
            }
            other => panic!("expected Bytes, got {other:?}"),
        },
        other => panic!("expected Ok(Bytes), got {other:?}"),
    }
}

#[test]
fn secp256k1_recover_yields_signer_pubkey() {
    let ok = run(SECP_SRC, "recover_matches",
        vec![Value::Bytes(secp_test_secret()), Value::Bytes(secp_test_digest())]);
    assert!(is_true(ok), "recovered pubkey must equal the signer's pubkey");
}

#[test]
fn secp256k1_sign_verify_roundtrip() {
    let ok = run(SECP_SRC, "verify_roundtrip",
        vec![Value::Bytes(secp_test_secret()), Value::Bytes(secp_test_digest())]);
    assert!(is_true(ok), "a valid signature must verify against the signer's pubkey");
}

#[test]
fn secp256k1_rejects_wrong_digest() {
    let other = hex::decode(
        "1111111111111111111111111111111111111111111111111111111111111111").unwrap();
    let bad = run(SECP_SRC, "verify_wrong_digest",
        vec![Value::Bytes(secp_test_secret()), Value::Bytes(secp_test_digest()),
             Value::Bytes(other)]);
    assert!(!is_true(bad), "signature must not verify against a different digest");
}

#[test]
fn secp256k1_sign_digest_rejects_non_32_byte_digest() {
    let v = run(SECP_SRC, "sign_of",
        vec![Value::Bytes(secp_test_secret()), Value::Bytes(vec![0u8; 31])]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err", "31-byte digest must Err"),
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn secp256k1_public_key_rejects_bad_length() {
    let v = run(SECP_SRC, "pub_of", vec![Value::Bytes(vec![1u8; 10])]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err", "10-byte secret must Err"),
        other => panic!("expected Result, got {other:?}"),
    }
}

#[test]
fn secp256k1_generate_without_grant_is_rejected_by_static_policy_walk() {
    use lex_runtime::policy::check_program;
    let prog = parse_source(SECP_SRC).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).expect("type-check");
    let bc = compile_program(&stages);
    let policy = Policy::pure(); // no [random] grant
    assert!(
        check_program(&bc, &policy).is_err(),
        "expected static policy walk to reject secp256k1_generate's [random] without grant"
    );
}

// ─── base58 — Solana / x402 encoding (#658) ──────────────────────────────────
const B58_SRC: &str = r#"
import "std.crypto" as crypto

fn enc(data :: Bytes) -> Str { crypto.base58_encode(data) }

fn dec(s :: Str) -> Result[Bytes, Str] { crypto.base58_decode(s) }

# round-trip: decode(encode(x)) == x
fn roundtrip(data :: Bytes) -> Bool {
  match crypto.base58_decode(crypto.base58_encode(data)) {
    Err(_) => false,
    Ok(back) => crypto.eq(data, back),
  }
}
"#;

#[test]
fn base58_encodes_32_zero_bytes_as_solana_default_pubkey() {
    // 32 zero bytes is Solana's "default"/system pubkey: 32 '1' chars.
    let v = run(B58_SRC, "enc", vec![Value::Bytes(vec![0u8; 32])]);
    assert_eq!(s(v), "1".repeat(32), "32 zero bytes encode to 32 leading '1's");
}

#[test]
fn base58_leading_zeros_become_ones() {
    // Each leading zero byte maps to one '1'; the rest encodes the value.
    let v = run(B58_SRC, "enc", vec![Value::Bytes(vec![0, 0, 0])]);
    assert_eq!(s(v), "111");
}

#[test]
fn base58_round_trip() {
    let ok = run(B58_SRC, "roundtrip",
        vec![Value::Bytes(b"\x00\x01\x02the quick brown fox".to_vec())]);
    assert!(is_true(ok), "base58 decode(encode(x)) must equal x");
}

#[test]
fn base58_decode_rejects_invalid_char() {
    // '0', 'O', 'I', 'l' are not in the base58 alphabet.
    let v = run(B58_SRC, "dec", vec![Value::Str("0OIl".into())]);
    match v {
        Value::Variant { name, .. } => assert_eq!(name, "Err", "invalid base58 must Err"),
        other => panic!("expected Result, got {other:?}"),
    }
}
