//! #1062 over HTTP: `POST /v1/merge/<id>/commit` records every sig the merge
//! decided, so a `take_ours` on a `delete_modify` (dst modified a fn, src
//! deleted it) keeps dst's version however the two histories replay.
//!
//! The hub's own head is read three ways after the commit — the incremental
//! `branch_head` (the hub took a snapshot at dst's tip when it published
//! there), a fresh full replay, and a store opened from scratch — and all
//! must agree with the resolution.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_store::{Store, DEFAULT_BRANCH};
use serde_json::json;
use tempfile::TempDir;

fn start_server() -> (SocketAddr, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok((200, _)) = try_http(&addr, "GET", "/v1/health", "") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "test server never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    (addr, tmp)
}

fn try_http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut s = TcpStream::connect_timeout(addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).map_err(|e| e.to_string())?;
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Err("empty response".into());
    }
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    Ok((status, body.to_string()))
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match try_http(addr, method, path, body) {
            Ok(r) => return r,
            Err(e) if std::time::Instant::now() >= deadline => panic!("http {method} {path}: {e}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn publish(addr: &SocketAddr, src: &str) {
    let (s, b) = http(addr, "POST", "/v1/publish", &json!({"source": src, "activate": true}).to_string());
    assert_eq!(s, 200, "publish: {b}");
}

const BASE: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\nfn keep(n :: Int) -> Int { n }\n";

/// One full run: main modifies `add`, feature deletes it, the merge is
/// resolved with `resolution` over HTTP. Returns the hub's head map.
fn run(resolution: &str) -> BTreeMap<String, String> {
    let (addr, tmp) = start_server();
    publish(&addr, BASE);
    let (s, b) = http(&addr, "POST", "/v1/branches", &json!({"name": "feature", "checkout": true}).to_string());
    assert_eq!(s, 201, "{b}");
    publish(&addr, "fn keep(n :: Int) -> Int { n }\n");
    let (s, b) = http(&addr, "POST", "/v1/branches/main/checkout", "");
    assert_eq!(s, 200, "{b}");
    publish(&addr, "fn add(x :: Int, y :: Int) -> Int { x + y + 1 }\nfn keep(n :: Int) -> Int { n }\n");

    let (s, b) = http(
        &addr,
        "POST",
        "/v1/merge/start",
        &json!({"src_branch": "feature", "dst_branch": DEFAULT_BRANCH}).to_string(),
    );
    assert_eq!(s, 200, "merge/start: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let merge_id = v["merge_id"].as_str().unwrap().to_string();
    let conflicts = v["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "{b}");
    assert_eq!(conflicts[0]["kind"], "delete_modify", "{b}");
    let conflict_id = conflicts[0]["conflict_id"].as_str().unwrap();

    let (s, b) = http(
        &addr,
        "POST",
        &format!("/v1/merge/{merge_id}/resolve"),
        &json!({"resolutions": [{"conflict_id": conflict_id, "resolution": {"kind": resolution}}]}).to_string(),
    );
    assert_eq!(s, 200, "merge/resolve: {b}");
    let (s, b) = http(&addr, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(s, 200, "merge/commit: {b}");
    let new_head = serde_json::from_str::<serde_json::Value>(&b).unwrap()["new_head_op"].as_str().unwrap().to_string();

    // Read it three ways: the hub's own store handle (snapshot-assisted), a
    // fresh handle, and the canonical replay of the merge op.
    let store = Store::open(tmp.path()).unwrap();
    let via_branch = store.branch_head(DEFAULT_BRANCH).unwrap();
    let via_replay = store.sig_map_at_op(&new_head).unwrap();
    assert_eq!(via_branch, via_replay, "{resolution}: branch_head != full replay of the merge op");
    let reopened = Store::open(tmp.path()).unwrap().branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(reopened, via_branch);
    via_branch
}

#[test]
fn take_ours_over_http_keeps_dsts_modified_fn_every_time() {
    let mut heads = Vec::new();
    for i in 0..5 {
        let head = run("take_ours");
        assert_eq!(head.len(), 2, "run {i}: add (modified) + keep expected: {head:?}");
        heads.push(head);
    }
    for (i, h) in heads.iter().enumerate() {
        assert_eq!(h, &heads[0], "run {i} differs from run 0");
    }
}

#[test]
fn take_theirs_over_http_deletes_the_fn_every_time() {
    for i in 0..3 {
        let head = run("take_theirs");
        assert_eq!(head.len(), 1, "run {i}: only keep expected: {head:?}");
    }
}
