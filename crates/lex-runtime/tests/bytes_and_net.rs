//! std.bytes (pure) + std.net.get/post (effectful).

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn run(src: &str, func: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn allow(effects: &[&str]) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = effects.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    p
}

// -- bytes --------------------------------------------------------------

#[test]
fn bytes_len_works() {
    let src = r#"
import "std.bytes" as bytes
fn n(b :: Bytes) -> Int { bytes.len(b) }
"#;
    assert_eq!(run(src, "n", vec![Value::Bytes(b"hello".to_vec())], Policy::pure()), Value::Int(5));
    assert_eq!(run(src, "n", vec![Value::Bytes(vec![])], Policy::pure()), Value::Int(0));
}

#[test]
fn bytes_eq_compares_content() {
    let src = r#"
import "std.bytes" as bytes
fn same(a :: Bytes, b :: Bytes) -> Bool { bytes.eq(a, b) }
"#;
    assert_eq!(run(src, "same", vec![
        Value::Bytes(b"abc".to_vec()), Value::Bytes(b"abc".to_vec())
    ], Policy::pure()), Value::Bool(true));
    assert_eq!(run(src, "same", vec![
        Value::Bytes(b"abc".to_vec()), Value::Bytes(b"abd".to_vec())
    ], Policy::pure()), Value::Bool(false));
}

#[test]
fn bytes_round_trip_through_str() {
    let src = r#"
import "std.bytes" as bytes
fn round_trip(s :: Str) -> Result[Str, Str] {
  bytes.to_str(bytes.from_str(s))
}
"#;
    let r = run(src, "round_trip", vec![Value::Str("hello, lex".into())], Policy::pure());
    assert_eq!(r, Value::Variant {
        name: "Ok".into(),
        args: vec![Value::Str("hello, lex".into())],
    });
}

#[test]
fn bytes_to_str_returns_err_on_invalid_utf8() {
    let src = r#"
import "std.bytes" as bytes
fn decode(b :: Bytes) -> Result[Str, Str] { bytes.to_str(b) }
"#;
    let r = run(src, "decode", vec![Value::Bytes(vec![0xff, 0xfe, 0xfd])], Policy::pure());
    let v = match r { Value::Variant { name, args } => (name, args), other => panic!("{other:?}") };
    assert_eq!(v.0, "Err");
}

#[test]
fn bytes_slice_works() {
    let src = r#"
import "std.bytes" as bytes
fn middle(b :: Bytes, lo :: Int, hi :: Int) -> Bytes { bytes.slice(b, lo, hi) }
"#;
    let r = run(src, "middle", vec![
        Value::Bytes(b"helloworld".to_vec()),
        Value::Int(2),
        Value::Int(7),
    ], Policy::pure());
    assert_eq!(r, Value::Bytes(b"llowo".to_vec()));
}

// -- net.get -----------------------------------------------------------

/// Spawn a tiny HTTP server that responds with a fixed body to one
/// request, then shuts down. Returns its port.
fn spawn_one_shot_server(body: &'static str) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body,
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (port, handle)
}

#[test]
fn net_get_returns_response_body() {
    let (port, handle) = spawn_one_shot_server("hello from server");
    let url = format!("http://127.0.0.1:{port}/");
    let src = r#"
import "std.net" as net
fn fetch(u :: Str) -> [net] Result[Str, Str] { net.get(u) }
"#;
    let r = run(src, "fetch", vec![Value::Str(url)], allow(&["net"]));
    let _ = handle.join();
    let (name, args) = match r {
        Value::Variant { name, args } => (name, args),
        other => panic!("expected Variant, got {other:?}"),
    };
    assert_eq!(name, "Ok");
    let body = match &args[0] { Value::Str(s) => s.clone(), _ => panic!() };
    assert!(body.contains("hello from server"), "body: {body}");
}

#[test]
fn net_get_blocked_without_policy() {
    // The policy walk rejects programs with [net] when net isn't allowed.
    let src = r#"
import "std.net" as net
fn fetch(u :: Str) -> [net] Result[Str, Str] { net.get(u) }
"#;
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("typecheck: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let policy = Policy::pure();
    let violations = lex_runtime::check_program(&bc, &policy).expect_err("must reject net");
    assert!(violations.iter().any(|v| v.effect.as_deref() == Some("net")));
}

#[test]
fn net_get_returns_err_for_bad_url() {
    let src = r#"
import "std.net" as net
fn fetch(u :: Str) -> [net] Result[Str, Str] { net.get(u) }
"#;
    let r = run(src, "fetch", vec![Value::Str("not-a-url".into())], allow(&["net"]));
    let (name, _) = match r { Value::Variant { name, args } => (name, args), other => panic!("{other:?}") };
    assert_eq!(name, "Err");
}

#[test]
fn net_get_accepts_https_scheme() {
    // Pre-HTTPS support, the runtime errored with "bad url: must start
    // with http://". This test pins the scheme acceptance: pointing at
    // a local TCP port that nobody serves on must produce a *transport*
    // error, not a URL parse error. Demonstrates net.get now accepts
    // https:// scheme even though TLS handshake will fail without a
    // peer.
    let src = r#"
import "std.net" as net
fn fetch(u :: Str) -> [net] Result[Str, Str] { net.get(u) }
"#;
    let r = run(src, "fetch",
        vec![Value::Str("https://127.0.0.1:1/".into())],
        allow(&["net"]));
    let (name, args) = match r {
        Value::Variant { name, args } => (name, args),
        other => panic!("{other:?}"),
    };
    assert_eq!(name, "Err", "expected Err from unreachable HTTPS endpoint");
    let msg = match &args[0] { Value::Str(s) => s.clone(), _ => panic!() };
    // Must be a transport-class error (connect refused / timeout /
    // tls), not the legacy URL-format rejection.
    assert!(
        !msg.contains("must start with"),
        "https:// must be accepted as a URL scheme; got: {msg}",
    );
}
