//! #992 over HTTP: a push whose head would bind a sig to a stage filed under
//! another sig is refused with **422**, not 500 — it is the client's data that
//! is wrong, not the server — and the body names the pair, the sig that owns
//! the stage, and the fix.
//!
//! Drives the real `op push` sequence against a loopback hub: stage blobs via
//! `/v1/stages/batch`, op records via `/v1/ops/batch` (accepted verbatim, as
//! they always are — history may legitimately contain such a pair), then the
//! ref half, `POST /v1/branches/main/head`, which is where the gate bites.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{Operation, OperationKind, OperationRecord, StageTransition};
use tempfile::TempDir;

const WITH_IO: &str = "fn serve(p :: Str) -> [io] Str { p }\n";
const WITH_FS: &str = "fn serve(p :: Str) -> [fs_read] Str { p }\n";

fn start_server() -> (SocketAddr, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    (addr, tmp)
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
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
    (
        status,
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

fn only_fn(src: &str) -> lex_ast::Stage {
    lex_ast::canonicalize_program(&lex_syntax::parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(_)))
        .expect("a fn")
}

fn ids(src: &str) -> (String, String) {
    let st = only_fn(src);
    (
        lex_ast::sig_id(&st).unwrap(),
        lex_ast::stage_id(&st).unwrap(),
    )
}

fn add(parent: Option<&str>, sig: &str, stage: &str) -> OperationRecord {
    OperationRecord::new(
        Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stage.into(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            parent
                .map(|p| p.to_string())
                .into_iter()
                .collect::<Vec<_>>(),
        ),
        StageTransition::Create {
            sig_id: sig.into(),
            stage_id: stage.into(),
        },
    )
}

/// History up to a clean head: both `serve` variants filed under their own
/// sigs. Returns (records, clean head op id).
fn clean_history() -> (Vec<OperationRecord>, String) {
    let (sig_io, stage_io) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    let a = add(None, &sig_io, &stage_io);
    let b = add(Some(&a.op_id), &sig_fs, &stage_fs);
    let head = b.op_id.clone();
    (vec![a, b], head)
}

/// The pre-#992 `ChangeEffectSig`: old sig bound to the new stage.
fn stranding_op(parent: &str) -> OperationRecord {
    let (sig_io, stage_io) = ids(WITH_IO);
    let (_, stage_fs) = ids(WITH_FS);
    OperationRecord::new(
        Operation::new(
            OperationKind::ChangeEffectSig {
                sig_id: sig_io.clone(),
                from_stage_id: stage_io.clone(),
                to_stage_id: stage_fs.clone(),
                from_effects: ["io".to_string()].into_iter().collect(),
                to_effects: ["fs_read".to_string()].into_iter().collect(),
                from_budget: None,
                to_budget: None,
                to_sig_id: None,
            },
            [parent.to_string()],
        ),
        StageTransition::Replace {
            sig_id: sig_io,
            from: stage_io,
            to: stage_fs,
        },
    )
}

fn push_objects(addr: &SocketAddr, ops: &[OperationRecord]) {
    let stages = serde_json::to_string(&vec![only_fn(WITH_IO), only_fn(WITH_FS)]).unwrap();
    let (s, b) = http(addr, "POST", "/v1/stages/batch", &stages);
    assert_eq!(s, 200, "stages: {b}");
    let (s, b) = http(
        addr,
        "POST",
        "/v1/ops/batch",
        &serde_json::to_string(ops).unwrap(),
    );
    assert_eq!(s, 200, "op records are accepted verbatim: {b}");
}

fn advance(addr: &SocketAddr, head: &str) -> (u16, serde_json::Value) {
    let body = serde_json::json!({ "head_op": head }).to_string();
    http(addr, "POST", "/v1/branches/main/head", &body)
}

#[test]
fn an_unsatisfiable_head_is_refused_with_422_naming_the_pair_and_the_fix() {
    let (addr, _tmp) = start_server();
    let (mut ops, clean) = clean_history();
    let bad = stranding_op(&clean);
    let bad_id = bad.op_id.clone();
    ops.push(bad);
    push_objects(&addr, &ops);

    let (status, body) = advance(&addr, &bad_id);
    assert_eq!(status, 422, "a client-data problem, never a 500: {body}");
    assert_eq!(body["error"], "UnsatisfiablePair", "{body}");

    let (sig_io, _) = ids(WITH_IO);
    let (sig_fs, stage_fs) = ids(WITH_FS);
    let d = &body["detail"];
    assert_eq!(d["sig_id"], sig_io.as_str(), "{body}");
    assert_eq!(d["stage_id"], stage_fs.as_str(), "{body}");
    assert_eq!(d["filed_under"], sig_fs.as_str(), "{body}");
    assert_eq!(
        d["hint"], "republish from source to retire the stranded entry (#995)",
        "{body}"
    );

    // Nothing moved: the branch does not exist yet, so its head is null.
    let (_, h) = http(&addr, "GET", "/v1/branches/main/head", "");
    assert!(
        h["head_op"].is_null(),
        "the refused advance must not create the branch: {h}"
    );
}

/// Negative control: the same history stopped one op earlier is a valid head
/// and advances normally.
#[test]
fn a_satisfiable_head_still_advances() {
    let (addr, _tmp) = start_server();
    let (ops, clean) = clean_history();
    push_objects(&addr, &ops);

    let (status, body) = advance(&addr, &clean);
    assert_eq!(status, 200, "{body}");
    let (_, h) = http(&addr, "GET", "/v1/branches/main/head", "");
    assert_eq!(h["head_op"], clean.as_str(), "{h}");
}
