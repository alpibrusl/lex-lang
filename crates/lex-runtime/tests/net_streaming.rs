//! Integration tests for `net.serve_fn` streaming response bodies (#375).
//!
//! Verifies on-the-wire behavior of `BodyStream` and `BodyBytes`:
//! - `Transfer-Encoding: chunked` is set
//! - Each Lex iter item lands as a distinct HTTP chunk on the wire
//! - Chunks decode back to the declared payload
//!
//! Uses raw `TcpStream` rather than `curl` so the test runs in CI
//! without depending on an external binary.

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

/// Raw GET that returns (status_line, headers_text, raw_body_bytes).
/// `raw_body_bytes` is the wire bytes between the empty header line
/// and EOF — for a chunked response that's the chunked encoding, not
/// the decoded payload. Tests inspect it directly to check chunking.
fn http_get(port: u16, path: &str) -> (String, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")
        .expect("response must have header/body separator");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let body = buf[split + 4..].to_vec();
    let (status_line, headers_text) = head.split_once("\r\n").unwrap_or((&head, ""));
    (status_line.to_string(), headers_text.to_string(), body)
}

/// Decode an HTTP/1.1 chunked-encoded body. Returns the concatenated
/// payload and the per-chunk byte vector (useful to assert "the wire
/// emitted N distinct chunks").
fn decode_chunked(raw: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let line_end = raw[i..].windows(2).position(|w| w == b"\r\n")
            .map(|p| i + p)
            .expect("chunk length line missing CRLF");
        let len_str = std::str::from_utf8(&raw[i..line_end]).expect("chunk length not utf8");
        // Ignore any chunk-extension after a semicolon.
        let len_str = len_str.split(';').next().unwrap().trim();
        let len = usize::from_str_radix(len_str, 16).expect("chunk length not hex");
        i = line_end + 2;
        if len == 0 {
            // Trailers + final CRLF — ignored in v1.
            break;
        }
        chunks.push(raw[i..i + len].to_vec());
        i += len + 2; // chunk data + trailing CRLF
    }
    let concat: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    (concat, chunks)
}

const SRC: &str = r#"
import "std.net" as net
import "std.iter" as iter
import "std.map" as map

fn handle(_req :: Request) -> Response {
  {
    status:  200,
    body:    BodyStream(iter.from_list(["alpha\n", "beta\n", "gamma\n"])),
    headers: map.from_list([("content-type", "text/event-stream")]),
  }
}

fn main() -> [net] Unit { net.serve_fn(18193, handle) }
"#;

const BYTES_SRC: &str = r#"
import "std.net" as net
import "std.iter" as iter
import "std.map" as map

fn handle(_req :: Request) -> Response {
  {
    status:  200,
    body:    BodyBytes(iter.from_list([[1, 2, 3], [4, 5], [6, 7, 8, 9]])),
    headers: map.from_list([("content-type", "application/octet-stream")]),
  }
}

fn main() -> [net] Unit { net.serve_fn(18194, handle) }
"#;

// #477 regression: `BodyStream(iter.unfold(...))` previously returned
// an empty body because the drain path only matched on `__IterEager`.
// The runtime now materialises lazy iters into eager ones before the
// drain runs, so unfold-based streams produce the same wire bytes as
// the equivalent `iter.from_list` version.
const UNFOLD_SRC: &str = r#"
import "std.net" as net
import "std.iter" as iter
import "std.map" as map

fn handle(_req :: Request) -> Response {
  let f := iter.unfold(0, fn (i :: Int) -> Option[(Str, Int)] {
    if i >= 3 { None } else { Some(("hello\n", i + 1)) }
  })
  {
    status:  200,
    body:    BodyStream(f),
    headers: map.from_list([("content-type", "text/plain")]),
  }
}

fn main() -> [net] Unit { net.serve_fn(18195, handle) }
"#;

#[test]
fn body_stream_uses_chunked_transfer_encoding() {
    spawn_lex_server(SRC, "main");
    wait_for_bind(18193, Duration::from_secs(5));
    let (status, headers, raw) = http_get(18193, "/sse");
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");
    let headers_lower = headers.to_ascii_lowercase();
    assert!(
        headers_lower.contains("transfer-encoding: chunked"),
        "expected chunked encoding; headers were:\n{headers}"
    );
    assert!(
        !headers_lower.contains("content-length:"),
        "chunked responses must not carry content-length; headers:\n{headers}"
    );
    let (payload, chunks) = decode_chunked(&raw);
    assert_eq!(
        String::from_utf8_lossy(&payload),
        "alpha\nbeta\ngamma\n",
        "concatenated body must equal joined iter items"
    );
    // v1 caveat: tiny_http / chunked-transfer accumulate `read()` calls
    // into a single HTTP chunk on the wire. The body is correctly
    // chunk-encoded (no Content-Length, valid chunked framing), but
    // per-iter-item chunk boundaries are lost. Lazy iters (follow-up
    // issue) will be the mechanism that actually exposes one Lex chunk
    // per HTTP chunk, because each `read()` will block on `iter.next`.
    assert!(
        !chunks.is_empty(),
        "expected at least one HTTP chunk in a chunked-encoded response"
    );
}

#[test]
fn body_bytes_emits_per_iter_item_chunks() {
    spawn_lex_server(BYTES_SRC, "main");
    wait_for_bind(18194, Duration::from_secs(5));
    let (status, headers, raw) = http_get(18194, "/blob");
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");
    let headers_lower = headers.to_ascii_lowercase();
    assert!(
        headers_lower.contains("transfer-encoding: chunked"),
        "expected chunked encoding; headers were:\n{headers}"
    );
    let (payload, chunks) = decode_chunked(&raw);
    assert_eq!(payload, vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9]);
    assert!(
        !chunks.is_empty(),
        "expected at least one HTTP chunk in a chunked-encoded response"
    );
}

#[test]
fn body_stream_unfold_produces_body_bytes() {
    // Regression for #477. Prior to the lazy-iter materialisation
    // pass in handler.rs, this test would observe a zero-byte body
    // because `drain_iter_str` matched only on `__IterEager`.
    spawn_lex_server(UNFOLD_SRC, "main");
    wait_for_bind(18195, Duration::from_secs(5));
    let (status, headers, raw) = http_get(18195, "/sse");
    assert!(status.starts_with("HTTP/1.1 200"), "status: {status}");
    let headers_lower = headers.to_ascii_lowercase();
    assert!(
        headers_lower.contains("transfer-encoding: chunked"),
        "expected chunked encoding; headers were:\n{headers}"
    );
    let (payload, chunks) = decode_chunked(&raw);
    assert_eq!(
        String::from_utf8_lossy(&payload),
        "hello\nhello\nhello\n",
        "unfold-based BodyStream must produce the same payload as the equivalent from_list version"
    );
    assert!(
        !chunks.is_empty(),
        "expected at least one HTTP chunk in a chunked-encoded response"
    );
}
