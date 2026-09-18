//! #949 phase 3: derived issue/project state over HTTP. A board is a view —
//! `GET /v1/issues` / `/v1/issues/<id>` / `/v1/projects` compute open /
//! in-progress / verified / blocked from the log; recording a verdict changes
//! what the next GET returns without anyone moving a card.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_store::issues::{record_issue_verdict, IssueEvaluation};
use lex_store::Store;
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

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
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
    (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
}

fn typed(title: &str, deps: BTreeSet<String>) -> Issue {
    Issue::with_timestamp(
        title,
        "",
        Acceptance::TypedDelta {
            api: vec![ApiEntry {
                name: "gcd".into(),
                signature: "(a :: Int, b :: Int) -> Int".into(),
                kind: ApiChangeKind::Added,
            }],
            examples: vec![],
        },
        None,
        deps,
        Some("nt".into()),
        1,
    )
}

fn state_of(issues: &serde_json::Value, id: &str) -> serde_json::Value {
    issues
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["issue"]["issue_id"] == id)
        .map(|s| s["state"].clone())
        .expect("issue listed")
}

#[test]
fn derived_state_follows_verdicts_and_dependencies() {
    let (srv, tmp) = start_server();
    let a = typed("add gcd", BTreeSet::new());
    let mut deps = BTreeSet::new();
    deps.insert(a.issue_id.clone());
    let b = typed("use gcd in lcm", deps);

    // Seed both issues through the sync surface.
    let batch = serde_json::to_string(&vec![a.clone(), b.clone()]).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/issues/batch", &batch);
    assert_eq!(status, 200);

    // Nothing carries either intent and A isn't verified: A open, B blocked on A.
    let (status, v) = http(&srv.addr, "GET", "/v1/issues", "");
    assert_eq!(status, 200, "{v}");
    assert_eq!(state_of(&v["issues"], &a.issue_id), "open");
    assert_eq!(state_of(&v["issues"], &b.issue_id), "blocked");
    let blocked_on = v["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["issue"]["issue_id"] == b.issue_id)
        .map(|s| s["blocked_on"].clone())
        .unwrap();
    assert_eq!(blocked_on, serde_json::json!([a.issue_id]));

    // Detail: state + (no) verdicts. Unknown id → 404. `/list` literal still
    // wins over the `<id>` prefix arm.
    let (status, d) = http(&srv.addr, "GET", &format!("/v1/issues/{}", a.issue_id), "");
    assert_eq!(status, 200, "{d}");
    assert_eq!(d["state"], "open");
    assert!(d["verdicts"].as_array().unwrap().is_empty());
    let (status, _) = http(&srv.addr, "GET", "/v1/issues/absent", "");
    assert_eq!(status, 404);
    let (status, l) = http(&srv.addr, "GET", "/v1/issues/list", "");
    assert_eq!(status, 200);
    assert_eq!(l["ids"].as_array().unwrap().len(), 2);

    // Projects: one project, one open + one blocked.
    let (status, p) = http(&srv.addr, "GET", "/v1/projects", "");
    assert_eq!(status, 200, "{p}");
    let nt = &p["projects"][0];
    assert_eq!(nt["name"], "nt");
    assert_eq!(nt["counts"]["open"], 1);
    assert_eq!(nt["counts"]["blocked"], 1);
    assert_eq!(nt["counts"]["verified"], 0);

    // Record a passing verdict for A (the gate does this; here directly
    // through a second handle on the same store) — nobody moves a card.
    let store = Store::open(tmp.path()).unwrap();
    record_issue_verdict(&store, &a, "op_test", &IssueEvaluation::Passed).unwrap();

    // The next GET derives it: A verified, so B is no longer blocked → open.
    let (_, v) = http(&srv.addr, "GET", "/v1/issues", "");
    assert_eq!(state_of(&v["issues"], &a.issue_id), "verified");
    assert_eq!(state_of(&v["issues"], &b.issue_id), "open");
    let (_, d) = http(&srv.addr, "GET", &format!("/v1/issues/{}", a.issue_id), "");
    assert_eq!(d["state"], "verified");
    assert_eq!(d["verdicts"].as_array().unwrap().len(), 1);
    assert_eq!(d["verdicts"][0]["result"]["result"], "passed");
    let (_, p) = http(&srv.addr, "GET", "/v1/projects", "");
    assert_eq!(p["projects"][0]["counts"]["verified"], 1);
    assert_eq!(p["projects"][0]["counts"]["open"], 1);
    assert_eq!(p["projects"][0]["counts"]["blocked"], 0);
}
