//! #837 piece A, the local-first half: `lex ws transform` writes a typed edit
//! straight through the op log of an on-disk store, over the SAME code path
//! (`lex_api::transform_http::apply_transform`) as `POST /v1/transform`.
//!
//! The parity test is the point: two byte-identical stores, one driven through
//! the real in-process lex-api handlers and one through the real `lex` binary,
//! given the same transform and the same (pinned) intent, must land the same
//! OpId. If the two doors ever diverge — a different gate, a different intent
//! shape, a different op — that test goes red.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_ast::{canonicalize_program, sig_id, stage_id, Stage};
use lex_store::{Store, DEFAULT_BRANCH};
use serde_json::{json, Value};
use tempfile::TempDir;

fn lex_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lex"))
}

// ── an identical store, built through the library (no intent, no clock) ────

const PICK_SRC: &str = "fn pick(n :: Int) -> Int { match n { 0 => 1, _ => 2 } }\n";
const ADD_TWO_SRC: &str = "fn add_two(n :: Int) -> Int { let x := n + 1; x + 1 }\n";
const COMBINE_SRC: &str = "fn combine(n :: Int, m :: Int) -> Int { (n * 2) + m }\n";

fn seed(dir: &Path, src: &str, name: &str) -> String {
    let store = Store::open(dir).unwrap();
    let stage: Stage = canonicalize_program(&lex_syntax::parse_source(src).unwrap())
        .into_iter()
        .find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == name))
        .unwrap();
    let (sig, stg) = (sig_id(&stage).unwrap(), stage_id(&stage).unwrap());
    store.publish(&stage).unwrap();
    let op = lex_vcs::Operation::new(
        lex_vcs::OperationKind::AddFunction {
            sig_id: sig.clone(),
            stage_id: stg.clone(),
            effects: Default::default(),
            budget_cost: None,
            in_file: None,
        },
        [],
    );
    store
        .apply_operation(
            DEFAULT_BRANCH,
            op,
            lex_vcs::StageTransition::Create { sig_id: sig, stage_id: stg.clone() },
        )
        .unwrap();
    stg
}

// ── the real in-process lex-api hub ─────────────────────────────────────────

fn start_hub(dir: &Path) -> SocketAddr {
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(dir.to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    return addr;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test server never became ready within 10s");
}

fn http_transform(addr: &SocketAddr, body: &Value) -> (u16, Value) {
    let body = body.to_string();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "POST /v1/transform HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

// ── running the real `lex` binary ───────────────────────────────────────────

fn run_lex(env_root: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = Command::new(lex_bin());
    cmd.current_dir(env_root)
        .env("HOME", env_root)
        .env_remove("LEX_STORE")
        .env_remove("LEX_INTENT_SESSION");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.args(args).output().unwrap_or_else(|e| panic!("spawning lex {args:?}: {e}"))
}

/// Run `lex --output json ws transform ...`; returns (exit code, envelope).
fn ws_transform(store: &Path, args: &[&str]) -> (i32, Value) {
    let mut full: Vec<&str> = vec!["--output", "json", "ws", "transform", "--store"];
    let s = store.to_str().unwrap().to_string();
    full.push(&s);
    full.extend_from_slice(args);
    let out = run_lex(store, &[], &full);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let v: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("non-JSON from lex ws ({e}); stdout: {text}\nstderr: {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap_or(-1), v)
}

fn data(v: &Value) -> &Value {
    v.get("data").unwrap_or(v)
}

fn head(dir: &Path, branch: &str) -> Option<String> {
    Store::open(dir).unwrap().get_branch(branch).unwrap().and_then(|b| b.head_op)
}

fn op_count(dir: &Path) -> usize {
    lex_vcs::OpLog::open(dir).unwrap().list_all().unwrap().len()
}

fn intent_of(dir: &Path, op_id: &str) -> lex_vcs::Intent {
    let rec = lex_vcs::OpLog::open(dir).unwrap().get(&op_id.to_string()).unwrap().unwrap();
    lex_vcs::IntentLog::open(dir).unwrap().get(&rec.op.intent_id.unwrap()).unwrap().unwrap()
}

/// One case: (source, fn name, transform kind, params sans `kind`).
fn cases() -> Vec<(&'static str, &'static str, &'static str, Value)> {
    vec![
        (PICK_SRC, "pick", "replace_match_arm", json!({
            "match_node": "n_0.2", "arm_index": 0,
            "new_body": {"node": "Literal", "value": {"kind": "Int", "value": 42}},
        })),
        (ADD_TWO_SRC, "add_two", "rename_local", json!({"let_node": "n_0.2", "new_name": "y"})),
        (ADD_TWO_SRC, "add_two", "inline_let", json!({"let_node": "n_0.2"})),
        (COMBINE_SRC, "combine", "extract_function", json!({
            "expr_node": "n_0.3.0",
            "spec": {"name": "double_n",
                     "params": [{"name": "n", "type": {"node": "Named", "name": "Int", "args": []}}],
                     "return_type": {"node": "Named", "name": "Int", "args": []}},
        })),
    ]
}

#[test]
fn ws_transform_lands_the_same_op_as_the_http_path_for_all_four_kinds() {
    for (src, name, kind, params) in cases() {
        let (http_dir, cli_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let stage_a = seed(http_dir.path(), src, name);
        let stage_b = seed(cli_dir.path(), src, name);
        assert_eq!(stage_a, stage_b);
        assert_eq!(head(http_dir.path(), "main"), head(cli_dir.path(), "main"), "identical stores");

        let mut t = params.clone();
        t["kind"] = json!(kind);
        t["from_stage_id"] = json!(stage_a);
        let addr = start_hub(http_dir.path());
        let (s, hv) = http_transform(
            &addr,
            &json!({"branch": "main",
                    "intent": {"prompt": "parity", "model": "test/m", "session": "sess-1"},
                    "transform": t}),
        );
        assert_eq!(s, 200, "{kind}: {hv}");

        let stage_id_s = stage_b.clone();
        let mut p = params.clone();
        p["from_stage_id"] = json!(stage_id_s);
        let (code, cv) = ws_transform(
            cli_dir.path(),
            &[
                "--branch", "main",
                "--intent-prompt", "parity", "--intent-model", "test/m", "--intent-session", "sess-1",
                kind, "--json", &p.to_string(),
            ],
        );
        assert_eq!(code, 0, "{kind}: {cv}");
        let cd = data(&cv);
        assert_eq!(cd["ok"], true, "{cv}");

        assert_eq!(cd["op_ids"], hv["op_ids"], "{kind}: CLI and HTTP must land the same OpIds");
        assert_eq!(cd["new_head"], hv["new_head"], "{kind}");
        assert_eq!(cd["intent"]["intent_id"], hv["intent"]["intent_id"], "{kind}");
        assert_eq!(cd["new_stage_id"], hv["new_stage_id"], "{kind}");
        assert_eq!(head(cli_dir.path(), "main"), head(http_dir.path(), "main"), "{kind}");
        let last = cd["op_id"].as_str().unwrap();
        let i = intent_of(cli_dir.path(), last);
        assert_eq!((i.prompt.as_str(), i.session_id.as_str()), ("parity", "sess-1"), "{kind}");
    }
}

#[test]
fn ws_transform_is_gated_exit_2_with_diagnostics_and_leaves_no_trace() {
    let dir = TempDir::new().unwrap();
    let stage = seed(dir.path(), PICK_SRC, "pick");
    let (h, ops) = (head(dir.path(), "main"), op_count(dir.path()));
    let intents = std::fs::read_dir(dir.path().join("intents")).map(|r| r.count()).unwrap_or(0);
    let p = json!({"from_stage_id": stage, "match_node": "n_0.2", "arm_index": 0,
                   "new_body": {"node": "Literal", "value": {"kind": "Str", "value": "oops"}}});
    let (code, v) = ws_transform(
        dir.path(),
        &["--branch", "main", "--intent-prompt", "break it", "--intent-session", "s",
          "replace_match_arm", "--json", &p.to_string()],
    );
    assert_eq!(code, 2, "{v}");
    assert_eq!(data(&v)["phase"], "type-check");
    assert!(!data(&v)["errors"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(head(dir.path(), "main"), h);
    assert_eq!(op_count(dir.path()), ops);
    let intents_after = std::fs::read_dir(dir.path().join("intents")).map(|r| r.count()).unwrap_or(0);
    assert_eq!(intents_after, intents, "a refused write records no intent");
}

#[test]
fn ws_transform_requires_a_branch_and_refuses_unknown_ones_and_bad_params() {
    let dir = TempDir::new().unwrap();
    let stage = seed(dir.path(), ADD_TWO_SRC, "add_two");
    let h = head(dir.path(), "main");
    let p = json!({"from_stage_id": stage, "let_node": "n_0.2"}).to_string();

    // No --branch: refused before anything is opened for writing.
    let (code, v) = ws_transform(dir.path(), &["inline_let", "--json", &p]);
    assert_eq!(code, 1, "{v}");
    assert!(v.to_string().contains("--branch is required"), "{v}");
    // Unknown branch.
    let (code, v) = ws_transform(dir.path(), &["--branch", "nope", "inline_let", "--json", &p]);
    assert_eq!(code, 1, "{v}");
    assert!(v.to_string().contains("unknown branch"), "{v}");
    // Unknown kind, and a param typo (deny_unknown_fields).
    let (code, _) = ws_transform(dir.path(), &["--branch", "main", "frobnicate", "--json", &p]);
    assert_eq!(code, 1);
    let typo = json!({"from_stage_id": stage, "let_nod": "n_0.2"}).to_string();
    let (code, _) = ws_transform(dir.path(), &["--branch", "main", "inline_let", "--json", &typo]);
    assert_eq!(code, 1);
    // A `kind` inside --json that disagrees with the positional.
    let clash = json!({"kind": "rename_local", "from_stage_id": stage, "let_node": "n_0.2"}).to_string();
    let (code, _) = ws_transform(dir.path(), &["--branch", "main", "inline_let", "--json", &clash]);
    assert_eq!(code, 1);
    // Blank prompt.
    let (code, _) = ws_transform(
        dir.path(),
        &["--branch", "main", "--intent-prompt", "  ", "inline_let", "--json", &p],
    );
    assert_eq!(code, 1);
    assert_eq!(head(dir.path(), "main"), h, "nothing above may move the head");
}

#[test]
fn ws_transform_without_an_intent_is_unattributed_and_honours_the_session_env() {
    let dir = TempDir::new().unwrap();
    let stage = seed(dir.path(), ADD_TWO_SRC, "add_two");
    let p = json!({"from_stage_id": stage, "let_node": "n_0.2"}).to_string();
    let store = dir.path().to_str().unwrap();
    // Same session rule as `lex publish`: LEX_INTENT_SESSION, else per-process.
    let out = run_lex(
        dir.path(),
        &[("LEX_INTENT_SESSION", "harness-run-7")],
        &["--output", "json", "ws", "transform", "--store", store, "--branch", "main", "inline_let", "--json", &p],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(data(&v)["intent"]["unattributed"], true, "{v}");
    let i = intent_of(dir.path(), data(&v)["op_id"].as_str().unwrap());
    assert_eq!(i.session_id, "harness-run-7");
    assert!(i.prompt.starts_with("(unattributed"), "{}", i.prompt);
}

#[test]
fn ws_is_registered_in_help() {
    let dir = TempDir::new().unwrap();
    let out = run_lex(dir.path(), &[], &["help"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("ws transform"));
}
