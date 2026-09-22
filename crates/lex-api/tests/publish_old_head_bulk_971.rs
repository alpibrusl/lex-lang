//! `POST /v1/publish` reads the branch head's old side in one bulk pass
//! (#971 follow-up to #1011), through the SigId the head names each stage
//! by. These pin the diff the handler computes from that old side:
//!
//! * an unchanged republish emits no ops;
//! * a one-function edit emits exactly one op, for that function;
//! * a type edit emits exactly one op (types are on the old side too, #895);
//! * two functions that differ only in name — so share one StageId — are
//!   both recognized as unchanged on republish, instead of the name the
//!   stage index doesn't point at being re-added every time (#826).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use serde_json::{json, Value};
use tempfile::TempDir;

fn start_server() -> (SocketAddr, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    (addr, tmp)
}

fn publish(addr: &SocketAddr, src: &str) -> Vec<Value> {
    let body = json!({"source": src, "activate": true}).to_string();
    let req = format!(
        "POST /v1/publish HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    assert!(head.contains(" 200 "), "publish failed: {head}\n{body}");
    let v: Value = serde_json::from_str(body).unwrap();
    v["ops"].as_array().cloned().unwrap_or_default()
}

fn kinds(ops: &[Value]) -> Vec<String> {
    ops.iter()
        .map(|o| o["kind"]["op"].as_str().unwrap_or("?").to_string())
        .collect()
}

const V1: &str = "type Pt = { x :: Int, y :: Int }\n\
                  fn double(n :: Int) -> Int { n * 2 }\n\
                  fn triple(n :: Int) -> Int { n * 3 }\n";

#[test]
fn unchanged_republish_emits_no_ops() {
    let (addr, _tmp) = start_server();
    assert!(!publish(&addr, V1).is_empty());
    let again = publish(&addr, V1);
    assert!(again.is_empty(), "republishing identical source must be a no-op: {again:?}");
}

#[test]
fn a_one_function_edit_emits_one_op() {
    let (addr, _tmp) = start_server();
    publish(&addr, V1);
    let v2 = V1.replace("n * 3", "n + n + n");
    let ops = publish(&addr, &v2);
    assert_eq!(ops.len(), 1, "{ops:?}");
    assert_eq!(kinds(&ops), vec!["modify_body"], "{ops:?}");
}

#[test]
fn a_type_edit_emits_one_op() {
    let (addr, _tmp) = start_server();
    publish(&addr, V1);
    let v2 = V1.replace("y :: Int }", "y :: Int, z :: Int }");
    let ops = publish(&addr, &v2);
    assert_eq!(ops.len(), 1, "{ops:?}");
}

#[test]
fn name_only_twins_are_both_unchanged_on_republish() {
    let (addr, _tmp) = start_server();
    let src = "fn inc_a(n :: Int) -> Int { n + 1 }\nfn inc_b(n :: Int) -> Int { n + 1 }\n";
    let first = publish(&addr, src);
    assert_eq!(first.len(), 2, "{first:?}");
    for _ in 0..3 {
        let again = publish(&addr, src);
        assert!(again.is_empty(), "twins must not be re-added on republish: {again:?}");
    }
}
