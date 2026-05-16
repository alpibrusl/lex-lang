//! Integration tests for `net.serve_routed` (#436). Spawns a Lex VM
//! running a `serve_routed` server in a background thread, then drives
//! it with raw HTTP requests over `TcpStream`. The server thread is
//! detached and dies when the test process exits.

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
        if TcpStream::connect_timeout(
            &("127.0.0.1", port).to_socket_addrs().unwrap().next().unwrap(),
            Duration::from_millis(200),
        ).is_ok() {
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

// One source serves all three tests below (single bind, multiple
// route shapes). Covers: static path, `:name` capture, and the
// fallback when nothing matches.
const SRC: &str = r#"
import "std.net" as net
import "std.map" as map

fn health(_req :: Request) -> Response {
  { status: 200, body: BodyStr("ok"), headers: map.from_list([]) }
}

fn show_user(req :: Request) -> Response {
  let id := match map.get(req.path_params, "id") {
    Some(s) => s,
    None    => "MISSING",
  }
  { status: 200, body: BodyStr("user=" + id), headers: map.from_list([]) }
}

fn nested(req :: Request) -> Response {
  let uid := match map.get(req.path_params, "uid") {
    Some(s) => s, None => "?",
  }
  let pid := match map.get(req.path_params, "pid") {
    Some(s) => s, None => "?",
  }
  { status: 200, body: BodyStr("u=" + uid + ";p=" + pid), headers: map.from_list([]) }
}

fn not_found(req :: Request) -> Response {
  { status: 404, body: BodyStr("nope " + req.method + " " + req.path),
    headers: map.from_list([]) }
}

fn main() -> [net] Unit {
  net.serve_routed(18200, [
    ("GET",  "/health",                       health),
    ("GET",  "/users/:id",                    show_user),
    ("GET",  "/users/:uid/posts/:pid",        nested),
  ], not_found)
}
"#;

fn ensure_started() {
    use std::sync::Once;
    static START: Once = Once::new();
    START.call_once(|| {
        spawn_lex_server(SRC, "main");
        wait_for_bind(18200, Duration::from_secs(5));
    });
}

#[test]
fn static_path_dispatches_to_matching_handler() {
    ensure_started();
    let (status, body) = http(18200, "GET", "/health");
    assert_eq!(status, 200, "GET /health should hit the health route");
    assert_eq!(body, "ok");
}

#[test]
fn colon_segment_captures_into_path_params() {
    ensure_started();
    let (status, body) = http(18200, "GET", "/users/42");
    assert_eq!(status, 200, "GET /users/42 should match /users/:id");
    assert_eq!(body, "user=42", "show_user should see id=42 in path_params");
}

#[test]
fn multiple_colon_segments_all_captured() {
    ensure_started();
    let (status, body) = http(18200, "GET", "/users/alice/posts/7");
    assert_eq!(status, 200);
    assert_eq!(body, "u=alice;p=7",
        "nested should see both :uid and :pid in path_params");
}

#[test]
fn unmatched_path_falls_back() {
    ensure_started();
    let (status, body) = http(18200, "GET", "/something/else");
    assert_eq!(status, 404, "fallback should run when no route matches");
    assert_eq!(body, "nope GET /something/else");
}

#[test]
fn wrong_method_on_known_path_falls_back() {
    ensure_started();
    // /health is registered as GET only; POST should miss every route
    // (no method-match) and land on the fallback.
    let (status, body) = http(18200, "POST", "/health");
    assert_eq!(status, 404,
        "POST /health should fall back: route is GET-only");
    assert_eq!(body, "nope POST /health");
}
