//! #930 phase 2b-1: the committed-lock sync endpoints — `POST /v1/locks/batch`
//! (send) and `POST /v1/locks/fetch` (receive) — carry a package head's
//! `lex.lock` over the same HTTP surface as `op push`/`pull`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    (Server { addr, _join: Some(join) }, tmp)
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

const LOCK: &str = "version = 1\n\n[[package]]\nname = \"lex-nt\"\nversion = \"0.1.3\"\nhead_op = \"op_dep\"\n";

#[test]
fn lock_batch_then_fetch_round_trips() {
    let (srv, _tmp) = start_server();

    // Send a committed lock for a head.
    let batch = serde_json::json!([{ "head_op": "op_head1", "lock": LOCK }]).to_string();
    let (status, body) = http(&srv.addr, "POST", "/v1/locks/batch", &batch);
    assert_eq!(status, 200, "batch: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["received"], 1);
    assert_eq!(v["added"], 1);

    // Fetch it back.
    let fetch = serde_json::json!({ "head_ops": ["op_head1", "op_absent"] }).to_string();
    let (status, body) = http(&srv.addr, "POST", "/v1/locks/fetch", &fetch);
    assert_eq!(status, 200, "fetch: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["locks"]["op_head1"], LOCK, "the committed lock must round-trip");
    assert!(
        v["locks"].get("op_absent").is_none(),
        "a head with no committed lock is omitted, not null"
    );
}

#[test]
fn fetch_of_unknown_head_returns_empty_map() {
    let (srv, _tmp) = start_server();
    let fetch = serde_json::json!({ "head_ops": ["nope"] }).to_string();
    let (status, body) = http(&srv.addr, "POST", "/v1/locks/fetch", &fetch);
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["locks"].as_object().unwrap().is_empty());
}

#[test]
fn malformed_lock_batch_is_rejected() {
    let (srv, _tmp) = start_server();
    // Missing `lock` field.
    let batch = serde_json::json!([{ "head_op": "op_head1" }]).to_string();
    let (status, _body) = http(&srv.addr, "POST", "/v1/locks/batch", &batch);
    assert_eq!(status, 400);
}
