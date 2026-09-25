//! #1060 over HTTP: a rename leaves one StageId under two sigs holding two
//! different ASTs (a StageId does not encode the name, #826), and the hub has
//! to cope with that on both halves of a sync.
//!
//! * The **ref half** (`POST /v1/branches/main/head`): a correct rename history
//!   must advance — the #992 gate is about a head naming a pair no store can
//!   hold, and `to -> body` is a pair every store can hold — while a rename
//!   whose `to` sig is not the one its body hashes to is still refused (422).
//! * The **fetch half** (`POST /v1/stages/fetch`): by bare id the hub can only
//!   answer with the one variant `stage_index` names, so a puller asking for
//!   the renamed variant got the pre-rename one. `pairs` resolves each entry
//!   through the sig it names.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{Operation, OperationKind, OperationRecord, StageTransition};
use tempfile::TempDir;

const BEFORE: &str = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
const AFTER: &str = "fn plus(x :: Int, y :: Int) -> Int { x + y }\n";

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
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
}

fn only_fn(src: &str) -> lex_ast::Stage {
    lex_ast::canonicalize_program(&lex_syntax::parse_source(src).expect("parse"))
        .into_iter()
        .find(|s| matches!(s, lex_ast::Stage::FnDecl(_)))
        .expect("a fn")
}

fn ids(src: &str) -> (String, String) {
    let st = only_fn(src);
    (lex_ast::sig_id(&st).unwrap(), lex_ast::stage_id(&st).unwrap())
}

fn name_of(stage: &serde_json::Value) -> String {
    stage["name"].as_str().unwrap_or_default().to_string()
}

/// `add` published, then renamed to `plus` with `rename_to` as the sig the
/// rename claims to bind. Returns (records, head op id).
fn rename_history(rename_to: &str) -> (Vec<OperationRecord>, String) {
    let (sig_add, stage) = ids(BEFORE);
    let create = OperationRecord::new(
        Operation::new(
            OperationKind::AddFunction {
                sig_id: sig_add.clone(),
                stage_id: stage.clone(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            Vec::<String>::new(),
        ),
        StageTransition::Create { sig_id: sig_add.clone(), stage_id: stage.clone() },
    );
    let rename = OperationRecord::new(
        Operation::new(
            OperationKind::RenameSymbol {
                from: sig_add.clone(),
                to: rename_to.to_string(),
                body_stage_id: stage.clone(),
                in_file: None,
            },
            [create.op_id.clone()],
        ),
        StageTransition::Rename {
            from: sig_add,
            to: rename_to.to_string(),
            body_stage_id: stage,
        },
    );
    let head = rename.op_id.clone();
    (vec![create, rename], head)
}

fn push_objects(addr: &SocketAddr, ops: &[OperationRecord]) {
    let stages = serde_json::to_string(&vec![only_fn(BEFORE), only_fn(AFTER)]).unwrap();
    let (s, b) = http(addr, "POST", "/v1/stages/batch", &stages);
    assert_eq!(s, 200, "stages: {b}");
    let (s, b) = http(addr, "POST", "/v1/ops/batch", &serde_json::to_string(ops).unwrap());
    assert_eq!(s, 200, "op records are accepted verbatim: {b}");
}

fn advance(addr: &SocketAddr, head: &str) -> (u16, serde_json::Value) {
    http(addr, "POST", "/v1/branches/main/head", &serde_json::json!({ "head_op": head }).to_string())
}

#[test]
fn the_two_variants_really_share_a_stage_id() {
    // The premise everything below rests on: a rename changes the sig, not the
    // stage id.
    let (sig_add, stage_add) = ids(BEFORE);
    let (sig_plus, stage_plus) = ids(AFTER);
    assert_ne!(sig_add, sig_plus);
    assert_eq!(stage_add, stage_plus);
}

#[test]
fn a_correct_rename_history_advances_the_head() {
    let (addr, _tmp) = start_server();
    let (sig_plus, _) = ids(AFTER);
    let (ops, head) = rename_history(&sig_plus);
    push_objects(&addr, &ops);

    let (status, body) = advance(&addr, &head);
    assert_eq!(status, 200, "a rename binds `to -> body`, which every store can hold: {body}");
    let (_, h) = http(&addr, "GET", "/v1/branches/main/head", "");
    assert_eq!(h["head_op"], head.as_str(), "{h}");
}

/// Negative control: the gate still bites on a rename that binds a sig its
/// body does not hash to.
#[test]
fn a_rename_binding_the_wrong_sig_is_still_refused() {
    let (addr, _tmp) = start_server();
    let (ops, head) = rename_history("not-the-sig-this-body-hashes-to");
    push_objects(&addr, &ops);

    let (status, body) = advance(&addr, &head);
    assert_eq!(status, 422, "a client-data problem, never a 500: {body}");
    assert_eq!(body["error"], "UnsatisfiablePair", "{body}");
    assert_eq!(body["detail"]["sig_id"], "not-the-sig-this-body-hashes-to", "{body}");
    let (_, h) = http(&addr, "GET", "/v1/branches/main/head", "");
    assert!(h["head_op"].is_null(), "the refused advance must not create the branch: {h}");
}

#[test]
fn fetch_by_pair_returns_each_variant_under_its_own_sig() {
    let (addr, _tmp) = start_server();
    let (sig_add, stage) = ids(BEFORE);
    let (sig_plus, _) = ids(AFTER);
    let (ops, _) = rename_history(&sig_plus);
    push_objects(&addr, &ops);

    // By bare id the hub can name only one variant, whichever `stage_index`
    // holds: this is the ambiguity that broke pull.
    let (s, by_id) = http(&addr, "POST", "/v1/stages/fetch", &serde_json::json!({ "ids": [stage] }).to_string());
    assert_eq!(s, 200, "{by_id}");
    assert_eq!(by_id["stages"].as_array().unwrap().len(), 1, "{by_id}");

    // By pair, each variant is returned as itself.
    let body = serde_json::json!({ "pairs": [[sig_plus, stage], [sig_add, stage]] }).to_string();
    let (s, by_pair) = http(&addr, "POST", "/v1/stages/fetch", &body);
    assert_eq!(s, 200, "{by_pair}");
    let names: Vec<String> = by_pair["stages"].as_array().unwrap().iter().map(name_of).collect();
    assert_eq!(names, vec!["plus".to_string(), "add".to_string()], "{by_pair}");

    // A pair the hub does not hold is omitted, not an error, and not answered
    // with some other sig's variant.
    let body = serde_json::json!({ "pairs": [["no-such-sig", stage]] }).to_string();
    let (s, none) = http(&addr, "POST", "/v1/stages/fetch", &body);
    assert_eq!(s, 200, "{none}");
    assert!(none["stages"].as_array().unwrap().is_empty(), "{none}");
}

/// A client that sends both keys (so an older hub, which reads only `ids`,
/// still answers) must get the pair-exact answer from this hub.
#[test]
fn pairs_win_when_ids_ride_along() {
    let (addr, _tmp) = start_server();
    let (_, stage) = ids(BEFORE);
    let (sig_plus, _) = ids(AFTER);
    let (ops, _) = rename_history(&sig_plus);
    push_objects(&addr, &ops);

    let body = serde_json::json!({ "ids": [stage], "pairs": [[sig_plus, stage]] }).to_string();
    let (s, v) = http(&addr, "POST", "/v1/stages/fetch", &body);
    assert_eq!(s, 200, "{v}");
    let names: Vec<String> = v["stages"].as_array().unwrap().iter().map(name_of).collect();
    assert_eq!(names, vec!["plus".to_string()], "the renamed variant, not the pre-rename one: {v}");
}
