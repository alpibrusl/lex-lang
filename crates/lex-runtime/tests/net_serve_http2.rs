//! `LEX_NET_HTTP2=1` switches the server's per-connection builder
//! from hyper's HTTP/1-only to hyper-util's auto::Builder, which
//! accepts both HTTP/1 and HTTP/2 (via preface detection — h2c /
//! prior-knowledge clients).
//!
//! This file is a separate test binary so the env-var change doesn't
//! race with other server tests that assume HTTP/1-only behaviour.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn spawn_lex_server(src: &str, entry: &str) {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".to_string()].into_iter().collect::<BTreeSet<_>>();
    let entry = entry.to_string();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy.clone()).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call(&entry, vec![]);
    });
}

fn wait_for_bind(port: u16, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    let mut backoff = Duration::from_millis(20);
    loop {
        if let Ok(s) = TcpStream::connect_timeout(
            &("127.0.0.1", port).to_socket_addrs().unwrap().next().unwrap(),
            Duration::from_millis(200),
        ) {
            drop(s);
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("server on :{port} did not bind within {timeout:?}");
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(200));
    }
}

/// HTTP/2 connection preface — fixed 24 bytes that any h2 / h2c
/// client sends first. The auto builder uses these bytes to decide
/// between HTTP/1 and HTTP/2 framing for the rest of the connection.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Server with HTTP/2 enabled must:
///   1. still serve normal HTTP/1.1 requests (auto builder is
///      backwards-compatible).
///   2. respond with HTTP/2 framing (binary SETTINGS frame, not
///      `HTTP/1.1 400`) when the client sends the H/2 preface.
#[test]
fn lex_net_http2_env_enables_auto_builder() {
    // Set BEFORE spawning so the server's env_http2() read sees `1`.
    // Single-test binary → no cross-test env race.
    // SAFETY: process is single-threaded at this point; no other thread
    // can observe the env mutation.
    unsafe {
        std::env::set_var("LEX_NET_HTTP2", "1");
    }

    let src = r#"
import "std.net" as net
import "std.str" as str

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  { status: 200, body: str.concat("ok ", req.path) }
}

fn main() -> [net] Nil { net.serve(18099, "handle") }
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18099, Duration::from_secs(5));

    // --- assertion 1: HTTP/1.1 still works ----------------------
    {
        let mut s = TcpStream::connect(("127.0.0.1", 18099)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(
            b"GET /hello HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).unwrap();
        assert!(
            buf.starts_with("HTTP/1.1 200"),
            "expected HTTP/1.1 200, got: {buf:?}"
        );
        assert!(
            buf.contains("ok /hello"),
            "expected handler body in response, got: {buf:?}"
        );
    }

    // --- assertion 2: HTTP/2 preface elicits H/2 framing --------
    {
        let mut s = TcpStream::connect(("127.0.0.1", 18099)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(H2_PREFACE).unwrap();
        // Server's first response on a fresh H/2 connection is a
        // SETTINGS frame: 9-byte header where byte 3 == 0x04
        // (SETTINGS type) and stream_id == 0. We just check that
        // the first 9 bytes don't look like an HTTP/1 status line.
        let mut buf = [0u8; 9];
        let n = s.read(&mut buf).unwrap_or(0);
        assert!(
            n >= 9,
            "expected at least 9 bytes of H/2 framing, got {n}"
        );
        // HTTP/1 reply would start with `H` `T` `T` `P` `/` `1`.
        // H/2 SETTINGS frame's type byte is at offset 3; payload
        // length lives in bytes 0..3 (big-endian u24).
        let frame_type = buf[3];
        assert_eq!(
            frame_type, 0x04,
            "expected SETTINGS frame (type=0x04), got header bytes: {buf:02x?} \
             (HTTP/1 would start with 0x48 0x54 0x54 0x50)"
        );
    }
}
