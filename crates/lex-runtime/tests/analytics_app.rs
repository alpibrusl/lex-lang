//! Integration tests for the analytics example: spawn the Lex server,
//! hit each endpoint over HTTP, assert on the JSON.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()
        .parent().unwrap()
        .to_path_buf()
}

fn spawn_analytics_server(port: u16) {
    let csv_abs = workspace_root().join("examples/orders.csv");
    let csv_path_lit = csv_abs.to_str().expect("utf-8 path");
    let src = include_str!("../../../examples/analytics_app.lex")
        .replace("net.serve(8090,", &format!("net.serve({port},"))
        // Per-request workers get a fresh DefaultHandler with no read_root,
        // so CWD-relative paths fail under `cargo test`. Pin the absolute path.
        .replace("examples/orders.csv", csv_path_lit);
    let prog = parse_source(&src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let mut policy = Policy::pure();
    policy.allow_effects = ["io".to_string(), "net".to_string()]
        .into_iter().collect::<BTreeSet<_>>();
    thread::spawn(move || {
        let handler = DefaultHandler::new(policy)
            .with_program(Arc::clone(&bc));
        let mut vm = Vm::with_handler(&bc, Box::new(handler));
        let _ = vm.call("main", vec![]);
    });
    thread::sleep(Duration::from_millis(200));
}

fn http(port: u16, path: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

#[test]
fn analytics_count_endpoint() {
    let port = 18201;
    spawn_analytics_server(port);
    let (status, body) = http(port, "/count");
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"count\":25"), "body: {body}");
}

#[test]
fn analytics_total_endpoint() {
    let port = 18202;
    spawn_analytics_server(port);
    let (status, body) = http(port, "/total_cents");
    assert_eq!(status, 200);
    assert!(body.contains("\"total_cents\":70030"), "body: {body}");
}

#[test]
fn analytics_regions_endpoint() {
    let port = 18203;
    spawn_analytics_server(port);
    let (status, body) = http(port, "/regions");
    assert_eq!(status, 200);
    // Distinct regions in dataset, in first-seen order: EU, US, APAC.
    assert!(body.contains("\"EU\""),   "body: {body}");
    assert!(body.contains("\"US\""),   "body: {body}");
    assert!(body.contains("\"APAC\""), "body: {body}");
}

#[test]
fn analytics_by_region_endpoint() {
    let port = 18204;
    spawn_analytics_server(port);

    let (status, body) = http(port, "/by_region/EU");
    assert_eq!(status, 200);
    assert!(body.contains("\"region\":\"EU\""),   "body: {body}");
    assert!(body.contains("\"count\":9"),         "body: {body}");
    assert!(body.contains("\"sum_cents\":21246"), "body: {body}");

    let (status, body) = http(port, "/by_region/US");
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":9"),         "body: {body}");
    assert!(body.contains("\"sum_cents\":27391"), "body: {body}");

    let (status, body) = http(port, "/by_region/APAC");
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":7"),         "body: {body}");
    assert!(body.contains("\"sum_cents\":21393"), "body: {body}");
}

#[test]
fn analytics_by_product_endpoint() {
    let port = 18205;
    spawn_analytics_server(port);

    let (status, body) = http(port, "/by_product/widget");
    assert_eq!(status, 200);
    assert!(body.contains("\"product\":\"widget\""), "body: {body}");
    assert!(body.contains("\"count\":12"),           "body: {body}");
    assert!(body.contains("\"sum_cents\":16443"),    "body: {body}");

    let (status, body) = http(port, "/by_product/gadget");
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":7"),         "body: {body}");
    assert!(body.contains("\"sum_cents\":23093"), "body: {body}");

    let (status, body) = http(port, "/by_product/gizmo");
    assert_eq!(status, 200);
    assert!(body.contains("\"count\":6"),         "body: {body}");
    assert!(body.contains("\"sum_cents\":30494"), "body: {body}");
}

#[test]
fn analytics_unknown_endpoint_404() {
    let port = 18206;
    spawn_analytics_server(port);
    let (status, body) = http(port, "/nope");
    assert_eq!(status, 404, "body: {body}");
}
