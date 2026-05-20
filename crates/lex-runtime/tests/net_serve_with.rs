//! Tests for `net.serve_with` / `net.serve_fn_with` / `net.serve_routed_with`
//! and `net.default_opts()` — the first-class ServeOpts record API
//! introduced in lex-lang#497 (replacing the LEX_NET_HTTP2 /
//! LEX_NET_INLINE_VM env-var gates with a typed config record).

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

fn http(port: u16, method: &str, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// `net.serve_with` accepts a ServeOpts record literal and routes HTTP/1
/// traffic to the handler exactly like `net.serve` does. Sanity check
/// that the new path doesn't lose request marshaling.
#[test]
fn serve_with_default_opts_handles_http1() {
    let src = r#"
import "std.net" as net
import "std.str" as str

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  { status: 200, body: str.concat("via opts: ", req.path) }
}

fn main() -> [net] Nil {
  let opts := net.default_opts()
  net.serve_with(18101, "handle", opts)
}
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18101, Duration::from_secs(5));
    let (status, body) = http(18101, "GET", "/hello");
    assert_eq!(status, 200);
    assert_eq!(body, "via opts: /hello");
}

/// `serve_with` with `http2: true` accepts both HTTP/1 (backwards-compat
/// via auto::Builder preface detection) and HTTP/2 prior-knowledge
/// connections. This is the opts-record equivalent of the LEX_NET_HTTP2
/// env-var test in `net_serve_http2.rs`.
#[test]
fn serve_with_http2_true_accepts_h2_preface() {
    let src = r#"
import "std.net" as net
import "std.str" as str

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  { status: 200, body: str.concat("h2-ready: ", req.path) }
}

fn main() -> [net] Nil {
  let opts := { http2: true, inline_vm: false, host: "0.0.0.0" }
  net.serve_with(18102, "handle", opts)
}
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18102, Duration::from_secs(5));

    // HTTP/1 still works.
    let (status, body) = http(18102, "GET", "/x");
    assert_eq!(status, 200);
    assert_eq!(body, "h2-ready: /x");

    // HTTP/2 preface elicits H/2 framing (SETTINGS, type 0x04).
    let mut s = TcpStream::connect(("127.0.0.1", 18102)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(H2_PREFACE).unwrap();
    let mut buf = [0u8; 9];
    let n = s.read(&mut buf).unwrap_or(0);
    assert!(n >= 9, "expected H/2 SETTINGS frame, got {n} bytes");
    assert_eq!(
        buf[3], 0x04,
        "expected SETTINGS (type=0x04), got header {buf:02x?}"
    );
}

/// `host: "127.0.0.1"` actually binds to loopback only (not 0.0.0.0).
/// We can't easily prove "binds only there" from a unit test, but we
/// can prove "binds *at* there" by hitting 127.0.0.1 directly.
#[test]
fn serve_with_custom_host_binds_correctly() {
    let src = r#"
import "std.net" as net

fn handle(_req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  { status: 200, body: "loopback" }
}

fn main() -> [net] Nil {
  let opts := { http2: false, inline_vm: false, host: "127.0.0.1" }
  net.serve_with(18103, "handle", opts)
}
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18103, Duration::from_secs(5));
    let (status, body) = http(18103, "GET", "/");
    assert_eq!(status, 200);
    assert_eq!(body, "loopback");
}

/// `serve_fn_with` accepts a closure handler + opts and dispatches
/// HTTP/1 traffic. Mirrors `serve_with` but exercises the closure
/// codepath (`serve_http_fn`). Uses the opaque `Request` / `Response`
/// types (same as existing `serve_fn` tests in net_streaming.rs).
#[test]
fn serve_fn_with_closure_handler_works() {
    let src = r#"
import "std.net" as net
import "std.map" as map

fn handle(_req :: Request) -> Response {
  { status: 200, body: BodyStr("closure-via-opts"), headers: map.new() }
}

fn main() -> [net] Nil {
  let opts := { http2: false, inline_vm: false, host: "0.0.0.0" }
  net.serve_fn_with(18104, handle, opts)
}
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18104, Duration::from_secs(5));
    let (status, body) = http(18104, "GET", "/foo");
    assert_eq!(status, 200);
    assert_eq!(body, "closure-via-opts");
}
