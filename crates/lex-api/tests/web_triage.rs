//! Web UI parity for human-triage actions (lex-tea v3c, #172).
//! One test in this binary, by design — `LEX_TEA_USER` is process-
//! global, so we set it once, exercise all four endpoints, and
//! restore it. Splitting into multiple `#[test]` functions would
//! race on the env var.

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
    _server_holder: Arc<()>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
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
    (Server { addr, _join: Some(join), _server_holder: Arc::new(()) }, tmp)
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
fn web_triage_pin_defer_block_unblock_end_to_end() {
    // Capture the prior value so the restoration at the end is
    // exact, not a blanket remove. Other tests in this binary may
    // not depend on it but a future refactor might.
    let prior = std::env::var("LEX_TEA_USER").ok();
    std::env::set_var("LEX_TEA_USER", "alice");

    let result = std::panic::catch_unwind(|| {
        let (srv, _tmp) = start_server();

        // Publish a stage to act on.
        let src = "fn foo(n :: Int) -> Int { n }\n";
        let (s, b) = http(&srv.addr, "POST", "/v1/publish",
            &json!({"source": src, "activate": false}).to_string());
        assert_eq!(s, 200, "publish: {b}");
        let v: serde_json::Value = serde_json::from_str(&b).unwrap();
        let stage_id = v["ops"][0]["kind"]["stage_id"].as_str().unwrap().to_string();

        // The stage page should show all four triage forms when
        // LEX_TEA_USER is set.
        let (s, page) = http(&srv.addr, "GET", &format!("/web/stage/{stage_id}"), "");
        assert_eq!(s, 200);
        for verb in ["pin", "defer", "block", "unblock"] {
            assert!(page.contains(&format!("/web/stage/{stage_id}/{verb}")),
                "stage page should expose {verb} form: not found");
        }
        assert!(page.contains("alice"), "actor name should appear: {page}");

        // POST defer → 303 redirect, no state change, defer
        // attestation persisted.
        let (s, _) = http(&srv.addr, "POST",
            &format!("/web/stage/{stage_id}/defer"),
            "reason=low%20priority");
        assert_eq!(s, 303, "defer should 303");

        // POST block → 303, then pin should refuse with 409.
        let (s, _) = http(&srv.addr, "POST",
            &format!("/web/stage/{stage_id}/block"),
            "reason=needs%20review");
        assert_eq!(s, 303, "block should 303");

        let (s, b) = http(&srv.addr, "POST",
            &format!("/web/stage/{stage_id}/pin"),
            "reason=ship");
        assert_eq!(s, 409, "pin should 409 when blocked: {b}");
        assert!(b.contains("blocked"), "409 body should mention blocked: {b}");

        // POST unblock → 303. Sleep ≥1s so the unblock has a
        // strictly later timestamp than the block.
        thread::sleep(Duration::from_millis(1100));
        let (s, _) = http(&srv.addr, "POST",
            &format!("/web/stage/{stage_id}/unblock"),
            "reason=cleared");
        assert_eq!(s, 303, "unblock should 303");

        // Now pin succeeds → 303.
        let (s, _) = http(&srv.addr, "POST",
            &format!("/web/stage/{stage_id}/pin"),
            "reason=ship");
        assert_eq!(s, 303, "pin should succeed after unblock");

        // The stage page should now show one of each kind.
        let (s, page) = http(&srv.addr, "GET", &format!("/web/stage/{stage_id}"), "");
        assert_eq!(s, 200);
        for kind in ["Defer(alice)", "Block(alice)", "Unblock(alice)", "Override(alice)"] {
            assert!(page.contains(kind), "stage page should list {kind}: not found");
        }

        // Missing reason → 400.
        let (s, _) = http(&srv.addr, "POST",
            &format!("/web/stage/{stage_id}/defer"),
            "");
        assert_eq!(s, 400);

        // Unknown stage → 404.
        let (s, _) = http(&srv.addr, "POST",
            "/web/stage/no_such_stage/defer",
            "reason=x");
        assert_eq!(s, 404);
    });

    // Restore env regardless of test outcome.
    match prior {
        Some(v) => std::env::set_var("LEX_TEA_USER", v),
        None => std::env::remove_var("LEX_TEA_USER"),
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
