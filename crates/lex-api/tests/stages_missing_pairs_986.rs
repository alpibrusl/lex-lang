//! #986: stage sync must be keyed by `(sig_id, stage_id)`, not the id alone.
//!
//! A `StageId` hashes the structural signature plus the implementation and
//! deliberately **not** the name (#826). So two functions that differ only in
//! name share one StageId while having two distinct SigIds — and two separate
//! ASTs, one stored under each sig. Rendering resolves through the pair.
//!
//! Every sync path used to ask only "do you have this stage id?", which can
//! answer *present* while the variant the head names is absent. That silently
//! defeated #968's closure reconciliation and produced an unrenderable release
//! (`lex-web@0.4.0`, archive 500 on `unknown stage_id`).
//!
//! The fixture below is the exact shape that breaks an id-only check: same
//! signature and body, different name.

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
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, body.to_string())
}

fn named(src: &str, name: &str) -> lex_ast::Stage {
    lex_ast::canonicalize_program(&lex_syntax::parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(fd) if fd.name == name))
        .expect("fn present")
}

/// Two names, one body and signature → **one StageId, two SigIds**.
fn twins() -> (lex_ast::Stage, lex_ast::Stage) {
    let a = named("fn alpha(n :: Int) -> Int { n + 1 }\n", "alpha");
    let b = named("fn beta(n :: Int) -> Int { n + 1 }\n", "beta");
    (a, b)
}

#[test]
fn the_fixture_really_is_two_sigs_sharing_one_stage_id() {
    let (a, b) = twins();
    assert_eq!(
        lex_ast::stage_id(&a),
        lex_ast::stage_id(&b),
        "the premise of #826: a StageId ignores the name"
    );
    assert_ne!(
        lex_ast::sig_id(&a),
        lex_ast::sig_id(&b),
        "…while the SigIds differ, so the ASTs live under separate sigs"
    );
}

#[test]
fn a_pair_whose_sig_is_absent_is_reported_missing() {
    let (addr, _tmp) = start_server();
    let (a, b) = twins();
    let stage = lex_ast::stage_id(&a).unwrap();
    let sig_a = lex_ast::sig_id(&a).unwrap();
    let sig_b = lex_ast::sig_id(&b).unwrap();

    // Seed only `alpha`. The shared stage id is now present on the server, but
    // only under alpha's sig.
    let (s, _) = http(&addr, "POST", "/v1/stages/batch", &serde_json::to_string(&vec![a]).unwrap());
    assert_eq!(s, 200);

    // The id-only question answers "present" — this is the blind spot.
    let (s, resp) = http(
        &addr, "POST", "/v1/stages/missing",
        &serde_json::json!({ "ids": [stage] }).to_string(),
    );
    assert_eq!(s, 200, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["missing"].as_array().unwrap().is_empty(),
        "id-only necessarily says present; that is why it is not enough: {resp}"
    );

    // The pair question tells the truth: beta's variant is absent.
    let (s, resp) = http(
        &addr, "POST", "/v1/stages/missing",
        &serde_json::json!({ "pairs": [[sig_a, stage], [sig_b, stage]] }).to_string(),
    );
    assert_eq!(s, 200, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let missing = v["missing"].as_array().unwrap();
    assert_eq!(
        missing.len(), 1,
        "exactly the absent sig's pair must be reported: {resp}"
    );
    assert_eq!(missing[0][0].as_str().unwrap(), sig_b, "and it must be beta's: {resp}");
    let _ = b;
}

#[test]
fn a_pair_that_is_present_is_not_reported_missing() {
    let (addr, _tmp) = start_server();
    let (a, b) = twins();
    let stage = lex_ast::stage_id(&a).unwrap();
    let sig_a = lex_ast::sig_id(&a).unwrap();
    let sig_b = lex_ast::sig_id(&b).unwrap();

    let both = serde_json::to_string(&vec![a, b]).unwrap();
    let (s, _) = http(&addr, "POST", "/v1/stages/batch", &both);
    assert_eq!(s, 200);

    let (s, resp) = http(
        &addr, "POST", "/v1/stages/missing",
        &serde_json::json!({ "pairs": [[sig_a, &stage], [sig_b, &stage]] }).to_string(),
    );
    assert_eq!(s, 200, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert!(
        v["missing"].as_array().unwrap().is_empty(),
        "both variants were sent, so neither is missing: {resp}"
    );
}

/// An older `op push` sends bare ids; it must still get an answer rather than
/// a 400, even though the answer is necessarily approximate.
#[test]
fn the_legacy_id_only_shape_still_works() {
    let (addr, _tmp) = start_server();
    let (a, _) = twins();
    let stage = lex_ast::stage_id(&a).unwrap();

    let (s, resp) = http(
        &addr, "POST", "/v1/stages/missing",
        &serde_json::json!({ "ids": [&stage, "deadbeef"] }).to_string(),
    );
    assert_eq!(s, 200, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    let missing: Vec<String> = v["missing"]
        .as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    assert!(missing.contains(&stage), "nothing seeded, so it is missing: {resp}");
    assert!(missing.contains(&"deadbeef".to_string()));
}
