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
        Value::Str(s) => s,
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
