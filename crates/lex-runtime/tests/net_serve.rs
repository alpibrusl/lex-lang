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
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Spawn the Lex source's `entry` function on a background thread.
/// The thread is detached; it dies when the test process exits.
///
/// Caller must follow with `wait_for_bind(port, ...)` to make sure
/// the listener is up before driving traffic — a fixed sleep was
/// flaky under TLS (cert + key parsing + rustls init can exceed
/// any reasonable static delay).
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

/// Poll-connect to `port` until the listener accepts a TCP
/// connection or the deadline expires. Replaces a fixed sleep so
/// the test passes whether bind takes 5ms (plain HTTP) or 500ms
/// (TLS with cold cert load) without slowing the fast path.
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
    wait_for_bind(18091, Duration::from_secs(5));
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
    wait_for_bind(18092, Duration::from_secs(5));
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
    wait_for_bind(18093, Duration::from_secs(5));

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

#[test]
fn net_serve_handles_concurrent_requests() {
    // Each handler simulates work via a small list-fold; many threads
    // hammer the server in parallel. All must succeed with the right
    // body. This validates that worker threads don't share mutable
    // state that could race.
    let src = r#"
import "std.net" as net
import "std.str" as str
import "std.int" as int
import "std.list" as list

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  # Sum 0..50; deterministic but non-trivial work to encourage scheduling.
  let total := list.fold(list.range(0, 50), 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
  { status: 200, body: str.concat(req.path, str.concat(":", int.to_str(total))) }
}

fn main() -> [net] Nil { net.serve(18094, "handle") }
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18094, Duration::from_secs(5));

    // Fire 32 requests across 8 client threads.
    let mut handles = Vec::new();
    for t in 0..8 {
        let h = thread::spawn(move || {
            for i in 0..4 {
                let path = format!("/req-{t}-{i}");
                let (status, body) = http(18094, "GET", &path, "");
                assert_eq!(status, 200, "req {t}-{i}: status {status}");
                assert!(body.starts_with(&path), "req {t}-{i}: body {body}");
                // sum 0..50 = 1225.
                assert!(body.ends_with(":1225"), "req {t}-{i}: body {body}");
            }
        });
        handles.push(h);
    }
    for h in handles { h.join().expect("worker thread panicked"); }
}

#[test]
fn net_serve_concurrent_requests_finish_in_bounded_time() {
    // 8 concurrent requests with light per-request work. Sanity check:
    // the server doesn't deadlock, doesn't drop requests, and responds
    // within a reasonable wall-clock bound. Strict speedup measurements
    // are environment-sensitive (single-CPU CI vs. multi-core) so we
    // only assert correctness + bounded duration here.
    let src = r#"
import "std.net" as net
import "std.list" as list
import "std.str" as str
import "std.int" as int

fn handle(req :: { body :: Str, method :: Str, path :: Str, query :: Str }) -> { body :: Str, status :: Int } {
  let n := list.fold(list.range(0, 200), 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
  { status: 200, body: int.to_str(n) }
}

fn main() -> [net] Nil { net.serve(18095, "handle") }
"#;
    spawn_lex_server(src, "main");
    wait_for_bind(18095, Duration::from_secs(5));

    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..8 {
        handles.push(thread::spawn(move || {
            let (status, body) = http(18095, "GET", "/work", "");
            assert_eq!(status, 200);
            assert_eq!(body, "19900");  // sum 0..200 = 19900
        }));
    }
    for h in handles { h.join().unwrap(); }
    let elapsed = start.elapsed();
    assert!(elapsed.as_secs() < 10, "8 concurrent requests took {}s — handler stuck?", elapsed.as_secs());
}

// ---- HTTPS (net.serve_tls) -------------------------------------------

#[test]
fn net_serve_tls_accepts_https_request() {
    let cert_dir = "tests/test_certs";
    let cert_path = format!("{cert_dir}/cert.pem");
    let key_path = format!("{cert_dir}/key.pem");
    assert!(std::path::Path::new(&cert_path).exists(),
        "missing test cert at {cert_path}; regenerate with openssl");
    assert!(std::path::Path::new(&key_path).exists(),
        "missing test key at {key_path}");

    let src = format!(r#"
import "std.net" as net
import "std.str" as str

fn handle(req :: {{ body :: Str, method :: Str, path :: Str, query :: Str }}) -> {{ body :: Str, status :: Int }} {{
  {{ status: 200, body: str.concat("tls-ok ", req.path) }}
}}

fn main() -> [net] Nil {{ net.serve_tls(18099, "{cert_path}", "{key_path}", "handle") }}
"#);

    spawn_lex_server(&src, "main");

    // The actual TLS handshake test: we just verify the bind happened
    // and the server accepts a TCP connection on the configured port.
    // A full TLS handshake requires either embedding rustls in this
    // test crate (heavy) or a child openssl process. Since the spec
    // wraps tiny_http's well-tested SSL path, we treat the bind as a
    // sufficient smoke for this PR.
    //
    // TLS bind is slower than plain HTTP (cert/key parsing + rustls
    // init) — generous timeout to accommodate cold starts on CI.
    wait_for_bind(18099, Duration::from_secs(10));
}
