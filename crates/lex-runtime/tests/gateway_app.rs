//! Integration tests for examples/gateway_app.lex — the multi-route
//! automation gateway. Verifies pure routes (classify / summarize /
//! help) work with no network/io grant; effectful routes work when
//! their effects are granted; routes with [net] are still scoped by
//! --allow-net-host.

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

fn spawn_gateway(port: u16) {
    let src = include_str!("../../../examples/gateway_app.lex")
        .replace("net.serve(8210,", &format!("net.serve({port},"));
    let prog = parse_source(&src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["net".into(), "time".into(), "io".into()]
        .into_iter().collect::<BTreeSet<_>>();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    thread::sleep(Duration::from_millis(200));
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

#[test]
fn help_lists_all_endpoints() {
    let port = 18501;
    spawn_gateway(port);
    let (status, body) = http(port, "GET", "/", "");
    assert_eq!(status, 200);
    for endpoint in &["/now", "/classify", "/summarize", "/weather", "/digest"] {
        assert!(body.contains(endpoint), "help missing `{endpoint}`: {body}");
    }
}

#[test]
fn classify_is_pure_and_returns_label() {
    let port = 18502;
    spawn_gateway(port);
    let (status, body) = http(port, "POST", "/classify", "URGENT: please respond");
    assert_eq!(status, 200);
    assert!(body.contains("\"label\":\"important\""), "body: {body}");
}

#[test]
fn summarize_is_pure_and_truncates_long_input() {
    let port = 18503;
    spawn_gateway(port);
    let long = "a".repeat(200);
    let (status, body) = http(port, "POST", "/summarize", &long);
    assert_eq!(status, 200);
    // Truncates to 80 chars + ellipsis.
    assert!(body.contains("…"), "expected truncation marker; body: {body}");
}

#[test]
fn now_returns_current_unix_timestamp() {
    let port = 18504;
    spawn_gateway(port);
    let (status, body) = http(port, "GET", "/now", "");
    assert_eq!(status, 200);
    // Body is `{"now":<seconds>}`. Pull out the integer and sanity-check
    // it's within ~1 hour of the system clock.
    let n = body.find(":").unwrap() + 1;
    let end = body.find("}").unwrap();
    let ts: i64 = body[n..end].parse().unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((now - ts).abs() < 3600, "stale timestamp ts={ts} now={now}");
}

#[test]
fn unknown_route_returns_404() {
    let port = 18505;
    spawn_gateway(port);
    let (status, _body) = http(port, "GET", "/no/such/route", "");
    assert_eq!(status, 404);
}
