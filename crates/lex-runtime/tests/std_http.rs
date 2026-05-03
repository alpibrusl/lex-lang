//! Integration tests for `std.http`. Closes #98.
//!
//! Drives the rich HTTP client against a tiny in-process server that
//! responds with a single canned response per accept. The pure
//! builders (`with_*`) and decoders (`{json,text}_body`) need no
//! network and use synthetic record values directly.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, MapKey, Value};
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
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn allow_net() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["net".to_string()].into_iter().collect::<BTreeSet<_>>();
    p
}

/// Spawn a one-shot HTTP server that replies with a fixed response
/// (status + body + headers) to one connection, then exits.
/// Captures the full request bytes (headers *and* body) for
/// assertion if `record_req` is `Some(channel)`. Returns the bound
/// port. The server keeps reading until it sees `\r\n\r\n` followed
/// by either the declared `Content-Length` bytes or EOF, so POST
/// bodies that arrive in a second packet are observed.
fn spawn_oneshot(
    response: &'static str,
    record_req: Option<std::sync::mpsc::Sender<String>>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let mut buf = Vec::with_capacity(8192);
            let mut tmp = [0u8; 4096];
            // Read the headers.
            let header_end = loop {
                match s.read(&mut tmp) {
                    Ok(0) => break buf.windows(4).position(|w| w == b"\r\n\r\n"),
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(p);
                        }
                    }
                    Err(_) => break buf.windows(4).position(|w| w == b"\r\n\r\n"),
                }
            };
            // If Content-Length was declared, finish reading the body.
            if let Some(end) = header_end {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let len = head.lines().find_map(|l| {
                    let lower = l.to_ascii_lowercase();
                    lower.strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                }).unwrap_or(0);
                let body_have = buf.len().saturating_sub(end + 4);
                let mut remaining = len.saturating_sub(body_have);
                while remaining > 0 {
                    match s.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            remaining = remaining.saturating_sub(n);
                        }
                        Err(_) => break,
                    }
                }
            }
            if let Some(tx) = record_req {
                let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
            }
            let _ = s.write_all(response.as_bytes());
        }
    });
    port
}

fn err_variant_name(v: &Value) -> Option<&str> {
    match v {
        Value::Variant { name, args } if name == "Err" && !args.is_empty() => match &args[0] {
            Value::Variant { name: inner, .. } => Some(inner.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn unwrap_ok_record(v: Value) -> indexmap::IndexMap<String, Value> {
    let args = match v {
        Value::Variant { name, args } if name == "Ok" => args,
        other => panic!("expected Ok, got {other:?}"),
    };
    match args.into_iter().next() {
        Some(Value::Record(r)) => r,
        other => panic!("expected Record inside Ok, got {other:?}"),
    }
}

// ---- pure builders --------------------------------------------------

const PURE_SRC: &str = r#"
import "std.http" as http
import "std.map" as map
import "std.option" as option

# Build a default `HttpRequest` with empty headers and no body.
fn base(u :: Str) -> HttpRequest {
  { method: "GET", url: u, headers: map.new(), body: None, timeout_ms: None }
}

fn url_after_query(u :: Str, k :: Str, v :: Str) -> Str {
  let req := base(u)
  let q := map.set(map.new(), k, v)
  let req2 := http.with_query(req, q)
  req2.url
}

fn header_value(u :: Str, k :: Str, v :: Str) -> Option[Str] {
  let req := base(u)
  let req2 := http.with_header(req, k, v)
  map.get(req2.headers, k)
}

fn auth_value(u :: Str, scheme :: Str, token :: Str) -> Option[Str] {
  let req := http.with_auth(base(u), scheme, token)
  map.get(req.headers, "authorization")
}

fn timeout_after(u :: Str, ms :: Int) -> Int {
  let req := http.with_timeout_ms(base(u), ms)
  match req.timeout_ms {
    Some(n) => n,
    None    => 0 - 1,
  }
}

# Header overwrite is case-insensitive: Content-Type and content-type
# are the same header, so the second `with_header` replaces the first.
fn double_set_returns_one(u :: Str) -> Int {
  let req := http.with_header(base(u), "Content-Type", "text/plain")
  let req2 := http.with_header(req, "content-type", "application/json")
  map.size(req2.headers)
}
"#;

#[test]
fn with_query_appends_encoded_params_to_url() {
    let v = run(
        PURE_SRC,
        "url_after_query",
        vec![
            Value::Str("https://example.com/api".into()),
            Value::Str("q".into()),
            Value::Str("hello world".into()),
        ],
        Policy::pure(),
    );
    match v {
        Value::Str(s) => assert_eq!(s, "https://example.com/api?q=hello%20world"),
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn with_query_extends_existing_query_string() {
    let v = run(
        PURE_SRC,
        "url_after_query",
        vec![
            Value::Str("https://example.com/api?a=1".into()),
            Value::Str("b".into()),
            Value::Str("2".into()),
        ],
        Policy::pure(),
    );
    match v {
        Value::Str(s) => assert_eq!(s, "https://example.com/api?a=1&b=2"),
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn with_header_records_value_under_lowercased_key() {
    let v = run(
        PURE_SRC,
        "header_value",
        vec![
            Value::Str("https://example.com/".into()),
            Value::Str("X-Trace".into()),
            Value::Str("abc123".into()),
        ],
        Policy::pure(),
    );
    // The lookup uses the original casing, but the map stores the
    // lowercased key — so `map.get(headers, "X-Trace")` returns None
    // while the lowercased lookup hits.
    assert_eq!(v, Value::Variant { name: "None".into(), args: vec![] });

    // Sanity check: the lowercased lookup hits (call again with the
    // already-lowercase key).
    let v2 = run(
        PURE_SRC,
        "header_value",
        vec![
            Value::Str("https://example.com/".into()),
            Value::Str("x-trace".into()),
            Value::Str("abc123".into()),
        ],
        Policy::pure(),
    );
    match v2 {
        Value::Variant { name, args } if name == "Some" => match &args[0] {
            Value::Str(s) => assert_eq!(s, "abc123"),
            other => panic!("{other:?}"),
        },
        other => panic!("expected Some, got {other:?}"),
    }
}

#[test]
fn with_auth_renders_scheme_and_token() {
    let v = run(
        PURE_SRC,
        "auth_value",
        vec![
            Value::Str("https://example.com/".into()),
            Value::Str("Bearer".into()),
            Value::Str("eyJ.tok.en".into()),
        ],
        Policy::pure(),
    );
    match v {
        Value::Variant { name, args } if name == "Some" => match &args[0] {
            Value::Str(s) => assert_eq!(s, "Bearer eyJ.tok.en"),
            other => panic!("{other:?}"),
        },
        other => panic!("expected Some(\"Bearer ...\"), got {other:?}"),
    }
}

#[test]
fn with_timeout_ms_sets_field() {
    let v = run(
        PURE_SRC,
        "timeout_after",
        vec![Value::Str("https://example.com/".into()), Value::Int(2500)],
        Policy::pure(),
    );
    assert_eq!(v, Value::Int(2500));
}

#[test]
fn with_header_is_case_insensitive_overwrite() {
    let v = run(
        PURE_SRC,
        "double_set_returns_one",
        vec![Value::Str("https://example.com/".into())],
        Policy::pure(),
    );
    assert_eq!(v, Value::Int(1));
}

// ---- decoders -------------------------------------------------------

#[test]
fn text_body_decodes_utf8_bytes() {
    let src = r#"
import "std.http" as http
import "std.map" as map

fn decode(b :: Bytes) -> Result[Str, HttpError] {
  let resp := { status: 200, headers: map.new(), body: b }
  http.text_body(resp)
}
"#;
    let v = run(
        src,
        "decode",
        vec![Value::Bytes(b"hello, world".to_vec())],
        Policy::pure(),
    );
    assert_eq!(
        v,
        Value::Variant { name: "Ok".into(), args: vec![Value::Str("hello, world".into())] },
    );
}

#[test]
fn text_body_returns_decode_error_on_invalid_utf8() {
    let src = r#"
import "std.http" as http
import "std.map" as map

fn decode(b :: Bytes) -> Result[Str, HttpError] {
  let resp := { status: 200, headers: map.new(), body: b }
  http.text_body(resp)
}
"#;
    let v = run(
        src,
        "decode",
        vec![Value::Bytes(vec![0xff, 0xfe, 0xfd])],
        Policy::pure(),
    );
    assert_eq!(err_variant_name(&v), Some("DecodeError"));
}

#[test]
fn json_body_parses_to_value() {
    let src = r#"
import "std.http" as http
import "std.map" as map

fn decode(b :: Bytes) -> Result[{ x :: Int, y :: Int }, HttpError] {
  let resp := { status: 200, headers: map.new(), body: b }
  http.json_body(resp)
}
"#;
    let v = run(
        src,
        "decode",
        vec![Value::Bytes(b"{\"x\":7,\"y\":11}".to_vec())],
        Policy::pure(),
    );
    let r = unwrap_ok_record(v);
    assert_eq!(r.get("x"), Some(&Value::Int(7)));
    assert_eq!(r.get("y"), Some(&Value::Int(11)));
}

#[test]
fn json_body_returns_decode_error_on_garbage() {
    let src = r#"
import "std.http" as http
import "std.map" as map

fn decode(b :: Bytes) -> Result[{ x :: Int }, HttpError] {
  let resp := { status: 200, headers: map.new(), body: b }
  http.json_body(resp)
}
"#;
    let v = run(
        src,
        "decode",
        vec![Value::Bytes(b"not json".to_vec())],
        Policy::pure(),
    );
    assert_eq!(err_variant_name(&v), Some("DecodeError"));
}

// ---- wire ops -------------------------------------------------------

#[test]
fn http_get_returns_response_with_status_and_body() {
    let port = spawn_oneshot(
        "HTTP/1.0 200 OK\r\n\
         Content-Length: 5\r\n\
         X-Server: lex-test\r\n\
         Connection: close\r\n\r\n\
         hello",
        None,
    );
    let url = format!("http://127.0.0.1:{port}/");
    let src = r#"
import "std.http" as http
fn fetch(u :: Str) -> [net] Result[HttpResponse, HttpError] { http.get(u) }
"#;
    let v = run(src, "fetch", vec![Value::Str(url)], allow_net());
    let r = unwrap_ok_record(v);
    assert_eq!(r.get("status"), Some(&Value::Int(200)));
    match r.get("body") {
        Some(Value::Bytes(b)) => assert_eq!(b, b"hello"),
        other => panic!("body was {other:?}"),
    }
    match r.get("headers") {
        Some(Value::Map(m)) => {
            assert_eq!(
                m.get(&MapKey::Str("x-server".into())),
                Some(&Value::Str("lex-test".into())),
            );
        }
        other => panic!("headers was {other:?}"),
    }
}

#[test]
fn http_post_sends_body_and_content_type() {
    let (tx, rx) = std::sync::mpsc::channel();
    let port = spawn_oneshot(
        "HTTP/1.0 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        Some(tx),
    );
    let url = format!("http://127.0.0.1:{port}/upload");
    let src = r#"
import "std.http" as http
fn push(u :: Str, b :: Bytes) -> [net] Result[HttpResponse, HttpError] {
  http.post(u, b, "application/octet-stream")
}
"#;
    let v = run(
        src,
        "push",
        vec![Value::Str(url), Value::Bytes(b"\x00\x01\x02hi".to_vec())],
        allow_net(),
    );
    let r = unwrap_ok_record(v);
    assert_eq!(r.get("status"), Some(&Value::Int(201)));
    let captured = rx.recv_timeout(std::time::Duration::from_secs(2))
        .expect("server should have observed the request");
    assert!(
        captured.contains("Content-Type: application/octet-stream")
            || captured.contains("content-type: application/octet-stream"),
        "expected content-type header in: {captured}",
    );
    assert!(captured.contains("\x00\x01\x02hi"), "expected body bytes in: {captured}");
}

#[test]
fn http_send_uses_request_record_headers_and_method() {
    let (tx, rx) = std::sync::mpsc::channel();
    let port = spawn_oneshot(
        "HTTP/1.0 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        Some(tx),
    );
    let url = format!("http://127.0.0.1:{port}/api");
    let src = r#"
import "std.http" as http
import "std.map" as map

# Builds a GET, sets Authorization via with_auth, sends it.
fn fetch_authed(u :: Str, token :: Str) -> [net] Result[HttpResponse, HttpError] {
  let req := { method: "GET", url: u, headers: map.new(), body: None, timeout_ms: None }
  http.send(http.with_auth(req, "Bearer", token))
}
"#;
    let v = run(
        src,
        "fetch_authed",
        vec![
            Value::Str(url),
            Value::Str("secret-jwt".into()),
        ],
        allow_net(),
    );
    let r = unwrap_ok_record(v);
    assert_eq!(r.get("status"), Some(&Value::Int(200)));
    let captured = rx.recv_timeout(std::time::Duration::from_secs(2))
        .expect("server should have observed the request");
    assert!(captured.starts_with("GET /api "), "unexpected request line: {captured}");
    assert!(
        captured.contains("authorization: Bearer secret-jwt"),
        "expected lowercased authorization header in: {captured}",
    );
}

#[test]
fn http_get_returns_network_error_on_unreachable() {
    // Port 1 has no listener; ureq surfaces this as a transport-class
    // error which we map to NetworkError.
    let src = r#"
import "std.http" as http
fn fetch(u :: Str) -> [net] Result[HttpResponse, HttpError] { http.get(u) }
"#;
    let v = run(
        src,
        "fetch",
        vec![Value::Str("http://127.0.0.1:1/".into())],
        allow_net(),
    );
    assert_eq!(err_variant_name(&v), Some("NetworkError"));
}

#[test]
fn http_get_blocked_when_host_outside_allow_net_host() {
    // The host gate is enforced before the wire op fires. A loud VM
    // error (rather than a structured Lex Err) is the same shape as
    // `net.get`'s host-gate violation today — the policy refuses to
    // run the call at all rather than letting a sandboxed program
    // see a recoverable Err.
    let mut p = allow_net();
    p.allow_net_host = vec!["api.openai.com".to_string()];
    let src = r#"
import "std.http" as http
fn fetch(u :: Str) -> [net] Result[HttpResponse, HttpError] { http.get(u) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(p);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let r = vm.call("fetch", vec![Value::Str("http://attacker.example.com/".into())]);
    let err = format!("{:?}", r.expect_err("expected host-gate refusal"));
    assert!(
        err.contains("attacker.example.com") && err.contains("allow-net-host"),
        "expected allow-net-host message, got {err}",
    );
}
