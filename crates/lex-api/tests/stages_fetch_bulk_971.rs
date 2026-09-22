//! #971: `/v1/stages/{fetch,batch,missing}` resolve their ids in one bulk
//! pass (`Store::get_asts_bulk`) instead of `get_ast` per id, which re-read
//! and re-parsed the whole stage index once per id — 256 index parses per
//! pull chunk, the dominant cost of the hub's slow `stages/fetch`. These
//! tests pin the observable behaviour of the bulk rewrite.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use tempfile::TempDir;

fn start_server() -> (SocketAddr, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(30));
    (addr, tmp)
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body
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

fn stages(n: usize) -> Vec<lex_ast::Stage> {
    let src: String = (0..n)
        .map(|i| format!("fn f{i}(x :: Int) -> Int {{ x + {i} }}\n"))
        .collect();
    lex_ast::canonicalize_program(&lex_syntax::parse_source(&src).expect("parse"))
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("bad json {body}: {e}"))
}

#[test]
fn batch_counts_added_skipped_and_in_batch_duplicates() {
    let (addr, _tmp) = start_server();
    let s = stages(3);
    let mut with_dup = s.clone();
    with_dup.push(s[0].clone());
    let (st, body) = http(
        &addr,
        "POST",
        "/v1/stages/batch",
        &serde_json::to_string(&with_dup).unwrap(),
    );
    assert_eq!(st, 200, "{body}");
    let v = json(&body);
    assert_eq!(
        (
            v["received"].as_u64(),
            v["added"].as_u64(),
            v["skipped"].as_u64()
        ),
        (Some(4), Some(3), Some(1))
    );

    // Re-push converges: everything already present.
    let (_, body) = http(
        &addr,
        "POST",
        "/v1/stages/batch",
        &serde_json::to_string(&s).unwrap(),
    );
    let v = json(&body);
    assert_eq!(
        (v["added"].as_u64(), v["skipped"].as_u64()),
        (Some(0), Some(3))
    );
}

#[test]
fn fetch_returns_present_stages_and_omits_unknown_ids() {
    let (addr, _tmp) = start_server();
    let s = stages(40);
    let (st, _) = http(
        &addr,
        "POST",
        "/v1/stages/batch",
        &serde_json::to_string(&s).unwrap(),
    );
    assert_eq!(st, 200);

    let mut want: Vec<String> = s
        .iter()
        .step_by(3)
        .map(|x| lex_ast::stage_id(x).unwrap())
        .collect();
    want.push("not-a-real-stage-id".into());
    let (st, body) = http(
        &addr,
        "POST",
        "/v1/stages/fetch",
        &serde_json::json!({ "ids": want }).to_string(),
    );
    assert_eq!(st, 200, "{body}");
    let got: Vec<lex_ast::Stage> = serde_json::from_value(json(&body)["stages"].clone()).unwrap();
    let got_ids: Vec<String> = got.iter().map(|x| lex_ast::stage_id(x).unwrap()).collect();
    assert_eq!(
        got_ids,
        want[..want.len() - 1].to_vec(),
        "request order, unknown id omitted"
    );
    for g in &got {
        let orig = s
            .iter()
            .find(|x| lex_ast::stage_id(x) == lex_ast::stage_id(g))
            .unwrap();
        assert_eq!(g, orig, "fetched AST round-trips");
    }
}

#[test]
fn legacy_missing_reports_only_absent_ids() {
    let (addr, _tmp) = start_server();
    let s = stages(5);
    let (st, _) = http(
        &addr,
        "POST",
        "/v1/stages/batch",
        &serde_json::to_string(&s[..3]).unwrap(),
    );
    assert_eq!(st, 200);
    let ids: Vec<String> = s.iter().map(|x| lex_ast::stage_id(x).unwrap()).collect();
    let (st, body) = http(
        &addr,
        "POST",
        "/v1/stages/missing",
        &serde_json::json!({ "ids": ids }).to_string(),
    );
    assert_eq!(st, 200, "{body}");
    let missing: Vec<String> = serde_json::from_value(json(&body)["missing"].clone()).unwrap();
    assert_eq!(missing, ids[3..].to_vec());
}
