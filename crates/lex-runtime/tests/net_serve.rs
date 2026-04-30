//! Integration tests for `net.serve`: spawn a Lex VM running an HTTP
//! handler in a background thread, then drive it with raw HTTP
//! requests over a TcpStream. The server thread is detached and
//! killed when the test process exits.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Spawn the Lex source's `entry` function on a background thread.
/// Returns the bound port (0 means "let OS choose"; we use a fixed
/// port chosen by the test).
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
    // Give the server a moment to bind.
    thread::sleep(Duration::from_millis(150));
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body,
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

/// Tiny echo handler — repeats the request method and path in the
/// response body. Useful for verifying the request marshaling layer.
const ECHO_SRC: &str = r#"
import "std.net" as net
import "std.str" as str

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  let line := str.concat(req.method, " ")
  let line2 := str.concat(line, req.path)
  { status: 200, body: line2 }
}

fn main() -> [net] Nil { net.serve(18091, "handle") }
"#;

#[test]
fn net_serve_dispatches_request_and_returns_response() {
    spawn_lex_server(ECHO_SRC, "main");
    let (status, body) = http(18091, "GET", "/hello", "");
    assert_eq!(status, 200);
    assert_eq!(body, "GET /hello");
}

#[test]
fn net_serve_handles_post_with_body() {
    // Reuse the same port → start a server on a different one.
    let src = r#"
import "std.net" as net
import "std.str" as str
fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  { status: 201, body: str.concat("got: ", req.body) }
}
fn main() -> [net] Nil { net.serve(18092, "handle") }
"#;
    spawn_lex_server(src, "main");
    let (status, body) = http(18092, "POST", "/widgets", "hello-payload");
    assert_eq!(status, 201);
    assert_eq!(body, "got: hello-payload");
}

#[test]
fn weather_app_responds_to_routes() {
    let src = include_str!("../../../examples/weather_app.lex");
    // Patch the example's port so the test owns its own.
    let src = src.replace("net.serve(8080,", "net.serve(18093,");
    spawn_lex_server(&src, "main");

    let (status, body) = http(18093, "GET", "/weather/SF", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"city\":\"SF\""), "body: {body}");
    assert!(body.contains("\"temp_c\":18"));

    let (status, body) = http(18093, "GET", "/forecast/Paris", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"city\":\"Paris\""));
    assert!(body.contains("day1"));

    let (status, body) = http(18093, "GET", "/health", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"));

    let (status, body) = http(18093, "GET", "/missing", "");
    assert_eq!(status, 404);
    assert!(body.contains("not found"));

    let (status, body) = http(18093, "POST", "/weather/SF", "");
    assert_eq!(status, 405);
    assert!(body.contains("method not allowed"));
}
