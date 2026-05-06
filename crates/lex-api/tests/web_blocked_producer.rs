//! Activity feed renders a `blocked` tag next to attestation
//! rows whose producer is in `<store>/policy.json` (#181).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use serde_json::json;
use tempfile::TempDir;

struct Server { addr: SocketAddr, _join: Option<thread::JoinHandle<()>> }

fn start_server(tmp: &TempDir) -> Server {
    let server = tiny_http::Server::http(("127.0.0.1", 0))
        .expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || {
        lex_api::serve_on(server, state);
    });
    thread::sleep(Duration::from_millis(20));
    Server { addr, _join: Some(join) }
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

#[test]
fn activity_feed_tags_blocked_producers() {
    // Publish a stage. The store-write gate auto-emits a TypeCheck
    // attestation whose producer tool is "lex publish".
    let tmp = TempDir::new().unwrap();
    let srv = start_server(&tmp);
    let src = "fn foo(n :: Int) -> Int { n }\n";
    let (s, _) = http(&srv.addr, "POST", "/v1/publish",
        &json!({"source": src, "activate": false}).to_string());
    assert_eq!(s, 200);

    // Initially, no policy.json → no `blocked` tag in the feed.
    let (s, b) = http(&srv.addr, "GET", "/", "");
    assert_eq!(s, 200);
    assert!(!b.contains(r#"class="tag blocked""#),
        "no policy.json → no blocked tag: {b}");

    // Drop a policy.json blocking "lex-store" — the producer
    // tool name the store-write gate uses for auto-TypeCheck.
    let policy = json!({
        "blocked_producers": [{
            "tool": "lex-store",
            "reason": "test",
            "blocked_at": 1,
        }]
    });
    std::fs::write(tmp.path().join("policy.json"),
        serde_json::to_vec_pretty(&policy).unwrap()).unwrap();

    // Now the feed should tag the row blocked.
    let (s, b) = http(&srv.addr, "GET", "/", "");
    assert_eq!(s, 200);
    assert!(b.contains(r#"class="tag blocked""#),
        "policy.json with `lex-store` should tag row as blocked: {b}");
}
