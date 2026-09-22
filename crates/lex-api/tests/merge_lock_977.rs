//! #977 over HTTP: `POST /v1/merge/<id>/commit` commits the merge op's OWN
//! lock — the union of both parents' pins — and refuses a same-package /
//! different-version conflict as a `409` whose body names the package and both
//! versions, leaving the destination branch where it was.

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
    // Poll until the serve loop answers (see m8.rs: a fixed sleep flakes
    // under parallel CI load).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok((200, _)) = try_http(&addr, "GET", "/v1/health", "") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "test server never became ready"
        );
        thread::sleep(Duration::from_millis(20));
    }
    (addr, tmp)
}

fn try_http(
    addr: &SocketAddr,
    method: &str,
    path: &str,
    body: &str,
) -> Result<(u16, String), String> {
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut s =
        TcpStream::connect_timeout(addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Err("empty response".into());
    }
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match try_http(addr, method, path, body) {
            Ok(r) => return r,
            Err(e) if std::time::Instant::now() >= deadline => {
                panic!("http {method} {path} failed after retries: {e}")
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn publish(addr: &SocketAddr, src: &str) {
    let (s, b) = http(
        addr,
        "POST",
        "/v1/publish",
        &json!({"source": src, "activate": true}).to_string(),
    );
    assert_eq!(s, 200, "publish: {b}");
}

fn lock_of(pins: &[(&str, &str)]) -> String {
    let mut s = String::from("version = 1\n");
    for (name, version) in pins {
        s.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nregistry = \"vcs.lexlang.org/lex-official/{name}\"\n\
             constraint = \"^{version}\"\nversion = \"{version}\"\nhead_op = \"op_{name}_{version}\"\n"
        ));
    }
    s
}

fn head_of(tmp: &TempDir, branch: &str) -> String {
    Store::open(tmp.path())
        .unwrap()
        .get_branch(branch)
        .unwrap()
        .and_then(|b| b.head_op)
        .unwrap()
}

/// Two branches diverge with disjoint edits (no code conflict), then each
/// head gets a pushed lock. Returns the open merge session id.
fn diverged_with_locks(
    addr: &SocketAddr,
    tmp: &TempDir,
    main_lock: &str,
    feat_lock: &str,
) -> String {
    publish(addr, "fn foo(n :: Int) -> Int { n }\n");
    let (s, b) = http(
        addr,
        "POST",
        "/v1/branches",
        &json!({"name": "feature", "checkout": true}).to_string(),
    );
    assert_eq!(s, 201, "create feature: {b}");
    publish(
        addr,
        "fn foo(n :: Int) -> Int { n }\nfn bar(n :: Int) -> Int { n + 1 }\n",
    );
    let (s, b) = http(addr, "POST", "/v1/branches/main/checkout", "");
    assert_eq!(s, 200, "checkout main: {b}");
    publish(
        addr,
        "fn foo(n :: Int) -> Int { n }\nfn baz(n :: Int) -> Int { n + 2 }\n",
    );

    // What `op push` does for each client-built head: commit its lock.
    let store = Store::open(tmp.path()).unwrap();
    store
        .set_committed_lock(&head_of(tmp, DEFAULT_BRANCH), main_lock)
        .unwrap();
    store
        .set_committed_lock(&head_of(tmp, "feature"), feat_lock)
        .unwrap();

    let (s, b) = http(
        addr,
        "POST",
        "/v1/merge/start",
        &json!({"src_branch": "feature", "dst_branch": DEFAULT_BRANCH}).to_string(),
    );
    assert_eq!(s, 200, "merge/start: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert!(
        v["conflicts"].as_array().unwrap().is_empty(),
        "disjoint edits: {b}"
    );
    v["merge_id"].as_str().unwrap().to_string()
}

#[test]
fn merge_commit_over_http_commits_the_union_lock() {
    let (addr, tmp) = start_server();
    let merge_id = diverged_with_locks(
        &addr,
        &tmp,
        &lock_of(&[("lex-nt", "1.0.0")]),
        &lock_of(&[("lex-nt", "1.0.0"), ("lex-mathx", "0.3.0")]),
    );
    let (s, b) = http(&addr, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(s, 200, "merge/commit: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let new_head = v["new_head_op"].as_str().unwrap();

    let lock = Store::open(tmp.path())
        .unwrap()
        .committed_lock(new_head)
        .unwrap()
        .expect("the merge op must carry its own lock (#977)");
    let lf = lex_syntax::lock::LockFile::from_toml(&lock).unwrap();
    assert_eq!(
        lf.entry("lex-nt").map(|e| e.version.as_str()),
        Some("1.0.0"),
        "{lock}"
    );
    assert_eq!(
        lf.entry("lex-mathx").map(|e| e.version.as_str()),
        Some("0.3.0"),
        "the source branch's new dependency must be in the merged lock: {lock}"
    );
}

#[test]
fn merge_commit_over_http_refuses_a_version_conflict_with_409() {
    let (addr, tmp) = start_server();
    let merge_id = diverged_with_locks(
        &addr,
        &tmp,
        &lock_of(&[("lex-nt", "1.0.0")]),
        &lock_of(&[("lex-nt", "2.0.0")]),
    );
    let main_before = head_of(&tmp, DEFAULT_BRANCH);
    let feat_before = head_of(&tmp, "feature");

    let (s, b) = http(&addr, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(
        s, 409,
        "a dependency conflict must be a 409, not a 500: {b}"
    );
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let d = &v["detail"];
    assert_eq!(d["kind"], "dependency_conflict", "{b}");
    assert_eq!(d["package"], "lex-nt", "{b}");
    assert_eq!(d["dst_version"], "1.0.0", "{b}");
    assert_eq!(d["src_version"], "2.0.0", "{b}");
    assert_eq!(d["dst_branch"], DEFAULT_BRANCH, "{b}");
    assert_eq!(d["src_branch"], "feature", "{b}");
    let msg = v["error"].as_str().unwrap();
    for needle in ["lex-nt", "1.0.0", "2.0.0"] {
        assert!(
            msg.contains(needle),
            "error message must name {needle}: {msg}"
        );
    }

    // Always-valid HEAD: neither branch moved.
    assert_eq!(
        head_of(&tmp, DEFAULT_BRANCH),
        main_before,
        "main must not advance"
    );
    assert_eq!(
        head_of(&tmp, "feature"),
        feat_before,
        "feature must not advance"
    );
}
