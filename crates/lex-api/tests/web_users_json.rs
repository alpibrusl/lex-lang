//! Web triage with `users.json` + `X-Lex-User` header (lex-tea
//! v3d, #172). One test by design — `LEX_TEA_USER` is process-
//! global so multiple tests in the same binary would race. The
//! header path doesn't have that risk but we exercise both here
//! to keep the cost flat.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use serde_json::json;
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
    // Write users.json *before* opening the store so a future
    // refactor that loads it on State::open still sees it.
    let users = json!({
        "users": [
            {"name": "alice", "role": "human"},
            {"name": "lexbot", "role": "agent"},
        ]
    });
    std::fs::write(tmp.path().join("users.json"),
        serde_json::to_vec_pretty(&users).unwrap()).unwrap();

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
    (Server { addr, _join: Some(join) }, tmp)
}

fn http_with_user(addr: &SocketAddr, method: &str, path: &str, body: &str,
    user: Option<&str>) -> (u16, String)
{
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let user_hdr = match user {
        Some(u) => format!("X-Lex-User: {u}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\n{user_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
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
fn web_users_json_gates_triage_actions() {
    let (srv, _tmp) = start_server();

    let src = "fn foo(n :: Int) -> Int { n }\n";
    let (s, b) = http_with_user(&srv.addr, "POST", "/v1/publish",
        &json!({"source": src, "activate": false}).to_string(), None);
    assert_eq!(s, 200, "publish: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let stage_id = v["ops"][0]["kind"]["stage_id"].as_str().unwrap().to_string();

    // No header → 403 (no actor identified, since LEX_TEA_USER is
    // unset in this test process).
    let (s, _) = http_with_user(&srv.addr, "POST",
        &format!("/web/stage/{stage_id}/defer"),
        "reason=x", None);
    assert_eq!(s, 403, "missing actor → 403");

    // Header with unknown actor → 403 (users.json lookup fails).
    let (s, b) = http_with_user(&srv.addr, "POST",
        &format!("/web/stage/{stage_id}/defer"),
        "reason=x", Some("eve"));
    assert_eq!(s, 403, "unknown actor → 403: {b}");
    assert!(b.contains("eve") || b.contains("users.json"),
        "403 body should explain why: {b}");

    // Header with known actor → 303.
    let (s, _) = http_with_user(&srv.addr, "POST",
        &format!("/web/stage/{stage_id}/defer"),
        "reason=low%20priority", Some("alice"));
    assert_eq!(s, 303, "known actor → 303");

    // The stage page should now show the defer attestation under
    // alice's name.
    let (s, page) = http_with_user(&srv.addr, "GET",
        &format!("/web/stage/{stage_id}"), "", Some("alice"));
    assert_eq!(s, 200);
    assert!(page.contains("Defer(alice)"),
        "stage page should list alice's defer: not found");
}
