//! Tests for the multi-tenant `State::new_with_tenant` constructor and
//! the `handle_with_auth` pre-routing hook added in PR #429.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use lex_api::handlers::{handle_with_auth, State};
use tempfile::TempDir;

// ---------- State::new_with_tenant ----------

#[test]
fn new_with_tenant_opens_under_prefix() {
    let tmp = TempDir::new().unwrap();
    let _state =
        State::new_with_tenant("acme", tmp.path().to_path_buf()).expect("valid tenant id");
    // Store::open creates `<root>/stages` and `<root>/traces`, so the
    // tenant root must exist after a successful open.
    assert!(
        tmp.path().join("acme/stages").is_dir(),
        "tenant store should be initialised at <store_root>/<tenant_id>/"
    );
    assert!(
        !tmp.path().join("stages").exists(),
        "store root should not be polluted at the parent level"
    );
}

#[test]
fn new_with_tenant_isolates_two_tenants() {
    let tmp = TempDir::new().unwrap();
    let _a = State::new_with_tenant("alpha", tmp.path().to_path_buf()).unwrap();
    let _b = State::new_with_tenant("beta", tmp.path().to_path_buf()).unwrap();
    assert!(tmp.path().join("alpha/stages").is_dir());
    assert!(tmp.path().join("beta/stages").is_dir());
}

#[test]
fn new_with_tenant_rejects_unsafe_ids() {
    let tmp = TempDir::new().unwrap();
    // Each of these would break tenant isolation if accepted:
    //   ""        — empty rejects before fs op
    //   ".."      — PathBuf::join("..") escapes one level up
    //   "../foo"  — same, with payload
    //   "/etc"    — PathBuf::join("/etc") replaces the root entirely
    //   "foo/bar" — nested traversal under another tenant
    //   "foo\\bar"— Windows-style separator
    //   "."       — dotfile / current-dir hazard
    //   ".hidden" — leading dot
    //   "a\0b"    — embedded NUL
    let bad = [
        "",
        "..",
        "../foo",
        "/etc",
        "foo/bar",
        "foo\\bar",
        ".",
        ".hidden",
        "a\0b",
    ];
    for id in &bad {
        let r = State::new_with_tenant(id, tmp.path().to_path_buf());
        assert!(r.is_err(), "tenant_id {id:?} should be rejected");
    }
    // Length cap: 65 ASCII chars must fail, 64 must pass.
    let too_long = "a".repeat(65);
    assert!(State::new_with_tenant(&too_long, tmp.path().to_path_buf()).is_err());
    let at_limit = "a".repeat(64);
    assert!(State::new_with_tenant(&at_limit, tmp.path().to_path_buf()).is_ok());
}

// ---------- handle_with_auth ----------

fn http(addr: &SocketAddr, path: &str, header: Option<(&str, &str)>) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let extra = match header {
        Some((k, v)) => format!("{k}: {v}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, body.to_string())
}

struct AuthServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<(String, bool)>>>,
}

/// Spin up a tiny_http server whose request loop calls `handle_with_auth`
/// with a closure that returns `true` iff the request carries a header
/// matching `expected_token`. Records the (path, auth_outcome) pair for
/// each request so the test can assert pass-through vs 401.
fn start_auth_server(expected_token: &'static str) -> AuthServer {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let stop_for_thread = Arc::clone(&stop);
    let seen_for_thread = Arc::clone(&seen);
    // Hold tmp alive until the server stops, by capturing it in the thread.
    thread::spawn(move || {
        let _hold = tmp;
        for request in server.incoming_requests() {
            if stop_for_thread.load(Ordering::Relaxed) {
                break;
            }
            let state = Arc::clone(&state);
            let seen = Arc::clone(&seen_for_thread);
            let _ = handle_with_auth(state, request, move |path, headers| {
                let ok = headers
                    .iter()
                    .any(|h| h.field.as_str().as_str() == "X-Auth"
                        && h.value.as_str() == expected_token);
                seen.lock().unwrap().push((path.to_string(), ok));
                ok
            });
        }
    });
    thread::sleep(Duration::from_millis(20));
    AuthServer { addr, stop, seen }
}

#[test]
fn handle_with_auth_returns_401_without_token() {
    let srv = start_auth_server("secret");
    let (status, body) = http(&srv.addr, "/v1/health", None);
    assert_eq!(status, 401, "missing token should 401");
    assert!(
        body.contains("\"error\":\"unauthorized\""),
        "401 body shape: {body:?}"
    );
    let seen = srv.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "/v1/health");
    assert!(!seen[0].1, "auth closure should have returned false");
    srv.stop.store(true, Ordering::Relaxed);
}

#[test]
fn handle_with_auth_passes_through_with_token() {
    let srv = start_auth_server("secret");
    let (status, body) = http(&srv.addr, "/v1/health", Some(("X-Auth", "secret")));
    assert_eq!(status, 200, "valid token should pass through, got: {body}");
    assert!(body.contains("\"ok\":true"), "health body: {body:?}");
    let seen = srv.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].1);
    srv.stop.store(true, Ordering::Relaxed);
}

#[test]
fn handle_with_auth_strips_query_string_before_auth() {
    let srv = start_auth_server("secret");
    let (_status, _body) = http(&srv.addr, "/v1/health?token=oops", None);
    let seen = srv.seen.lock().unwrap();
    assert_eq!(seen[0].0, "/v1/health", "auth path should not include query string");
    srv.stop.store(true, Ordering::Relaxed);
}
