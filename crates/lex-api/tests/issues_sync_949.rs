//! #949 phase 1: typed issues sync over the same HTTP surface as stages,
//! intents and locks — `POST /v1/issues/batch` (send), `GET /v1/issues/list`
//! (every id; open issues aren't reachable from any op, so a puller lists the
//! whole log), `POST /v1/issues/fetch` (receive by id).

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{Acceptance, ApiChangeKind, ApiEntry, Issue};
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

fn gcd_issue() -> Issue {
    Issue::with_timestamp(
        "add gcd",
        "number theory needs a gcd",
        Acceptance::TypedDelta {
            api: vec![ApiEntry {
                name: "gcd".into(),
                signature: "(Int, Int) -> Int".into(),
                kind: ApiChangeKind::Added,
            }],
            examples: vec!["gcd(12, 8) == 4".into()],
        },
        Some("op_base".into()),
        BTreeSet::new(),
        None,
        1,
    )
}

#[test]
fn issue_batch_list_fetch_round_trips() {
    let (srv, _tmp) = start_server();
    let issue = gcd_issue();

    // Send.
    let batch = serde_json::to_string(&vec![issue.clone()]).unwrap();
    let (status, body) = http(&srv.addr, "POST", "/v1/issues/batch", &batch);
    assert_eq!(status, 200, "batch: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["received"], 1);
    assert_eq!(v["added"], 1);

    // Idempotent re-send: content-addressed, nothing new.
    let (status, body) = http(&srv.addr, "POST", "/v1/issues/batch", &batch);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["added"], 0, "re-sending the same issue must add nothing");

    // List every id (open issues aren't reachable from ops).
    let (status, body) = http(&srv.addr, "GET", "/v1/issues/list", "");
    assert_eq!(status, 200, "list: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ids"], serde_json::json!([issue.issue_id]));

    // Fetch by id, byte-faithful round trip incl. the shape tag.
    let fetch = serde_json::json!({ "ids": [issue.issue_id, "absent"] }).to_string();
    let (status, body) = http(&srv.addr, "POST", "/v1/issues/fetch", &fetch);
    assert_eq!(status, 200, "fetch: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let got: Vec<Issue> = serde_json::from_value(v["issues"].clone()).unwrap();
    assert_eq!(got, vec![issue], "absent id omitted, present one identical");
}

#[test]
fn malformed_issue_batch_is_rejected() {
    let (srv, _tmp) = start_server();
    let (status, _) = http(&srv.addr, "POST", "/v1/issues/batch", r#"[{"title":"no acceptance"}]"#);
    assert_eq!(status, 400);
}

/// #949 phase 5: the id is the content hash. A record whose `issue_id` does
/// not match its content is refused (400) and nothing in the batch is filed —
/// otherwise a bad record could squat on a real issue's id.
#[test]
fn issue_with_inconsistent_id_is_refused_and_nothing_is_filed() {
    let (srv, _tmp) = start_server();
    let good = gcd_issue();
    let mut tampered = gcd_issue();
    tampered.title = "not the gcd issue".into(); // content changed, id kept
    assert!(!tampered.id_is_consistent());

    let batch = serde_json::to_string(&vec![good.clone(), tampered]).unwrap();
    let (status, body) = http(&srv.addr, "POST", "/v1/issues/batch", &batch);
    assert_eq!(status, 400, "tampered id must be refused: {body}");
    assert!(body.contains("does not match its content"), "{body}");

    // All-or-nothing: the good record in the same batch was not filed either.
    let (status, body) = http(&srv.addr, "GET", "/v1/issues/list", "");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["ids"].as_array().unwrap().is_empty(), "nothing filed: {body}");
}

#[test]
fn empty_log_lists_nothing() {
    let (srv, _tmp) = start_server();
    let (status, body) = http(&srv.addr, "GET", "/v1/issues/list", "");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["ids"].as_array().unwrap().is_empty());
}
