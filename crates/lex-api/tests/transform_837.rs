//! #837 piece A: the typed-transform write surface.
//!
//! `POST /v1/transform` lands a typed op (one of #280's four transforms)
//! through the gated op-log write path, attributed to an intent, on a branch
//! the request names. These tests drive the real handlers over loopback (the
//! in-process test-hub pattern of `op_push.rs`) and read the result back from
//! the op log — through the HTTP op endpoints and directly off disk.
//!
//! What they pin:
//!   * each of the four transforms lands its typed op with the supplied intent
//!     attached, the head advances and the resulting head still type-checks;
//!   * a transform that breaks the type-check is 422 with diagnostics, the
//!     head is unchanged, and NO op and NO intent was written;
//!   * unknown branch 404, missing `branch` 400 — and a supplied `branch` is
//!     honoured even when the server's global current branch is a different
//!     one, for `/v1/transform` AND `/v1/patch` (the old `/v1/patch` flaw);
//!   * same transform + same intent on two identical stores => the same OpId;
//!   * `/v1/patch` without `branch`/`intent` behaves exactly as before.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use serde_json::{json, Value};
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
    wait_until_serving(&addr);
    (Server { addr, _join: Some(join) }, tmp)
}

/// Don't race the accept loop (#1044): poll `/v1/health` until it answers.
fn wait_until_serving(addr: &SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let probe = b"GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    while std::time::Instant::now() < deadline {
        if let Ok(mut s) = TcpStream::connect_timeout(addr, Duration::from_millis(200)) {
            s.set_read_timeout(Some(Duration::from_millis(200))).ok();
            if s.write_all(probe).is_ok() {
                let mut buf = [0u8; 16];
                if s.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/1.1 200") {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("test server never became ready within 10s");
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

fn post(srv: &Server, path: &str, body: &Value) -> (u16, Value) {
    let (s, b) = http(&srv.addr, "POST", path, &body.to_string());
    let v = serde_json::from_str(&b).unwrap_or_else(|e| panic!("non-JSON body ({e}): {b}"));
    (s, v)
}

// ── fixtures ────────────────────────────────────────────────────────────────

const PICK_SRC: &str = "fn pick(n :: Int) -> Int { match n { 0 => 1, _ => 2 } }\n";
const ADD_TWO_SRC: &str = "fn add_two(n :: Int) -> Int { let x := n + 1; x + 1 }\n";
const COMBINE_SRC: &str = "fn combine(n :: Int, m :: Int) -> Int { (n * 2) + m }\n";

/// A server holding `src` published on `main` (also the current branch).
fn hub_with(src: &str) -> (Server, TempDir, String) {
    let (srv, tmp) = start_server();
    let (s, v) = post(&srv, "/v1/publish", &json!({"source": src, "activate": true}));
    assert_eq!(s, 200, "publish: {v}");
    let stage_id = v["ops"][0]["kind"]["stage_id"].as_str().unwrap().to_string();
    (srv, tmp, stage_id)
}

fn head_of(srv: &Server, branch: &str) -> Value {
    let (s, b) = http(&srv.addr, "GET", &format!("/v1/branches/{branch}/head"), "");
    assert_eq!(s, 200, "head: {b}");
    serde_json::from_str::<Value>(&b).unwrap()["head_op"].clone()
}

/// Every op record on disk (reachable or not) — the "nothing was written"
/// assertions compare this before/after a refused write.
fn op_count(tmp: &TempDir) -> usize {
    lex_vcs::OpLog::open(tmp.path()).unwrap().list_all().unwrap().len()
}

fn intent_count(tmp: &TempDir) -> usize {
    match std::fs::read_dir(tmp.path().join("intents")) {
        Ok(rd) => rd.filter_map(|e| e.ok()).count(),
        Err(_) => 0,
    }
}

fn op_record(tmp: &TempDir, op_id: &str) -> lex_vcs::OperationRecord {
    lex_vcs::OpLog::open(tmp.path())
        .unwrap()
        .get(&op_id.to_string())
        .unwrap()
        .unwrap_or_else(|| panic!("op {op_id} not in the log"))
}

fn intent_of(tmp: &TempDir, op_id: &str) -> lex_vcs::Intent {
    let id = op_record(tmp, op_id).op.intent_id.expect("op carries an intent id");
    lex_vcs::IntentLog::open(tmp.path())
        .unwrap()
        .get(&id)
        .unwrap()
        .expect("the op's intent is in the intent log")
}

fn intent(prompt: &str, session: &str) -> Value {
    json!({"prompt": prompt, "model": "test/model", "session": session})
}

fn transform(branch: &str, intent: Value, t: Value) -> Value {
    json!({"branch": branch, "intent": intent, "transform": t})
}

fn replace_arm(stage_id: &str, value: Value) -> Value {
    json!({
        "kind": "replace_match_arm",
        "from_stage_id": stage_id,
        "match_node": "n_0.2",
        "arm_index": 0,
        "new_body": {"node": "Literal", "value": value},
    })
}

/// The head still type-checks: publishing it back is a no-op the gate accepts,
/// and every stage the head binds loads. (The write-time gate is what makes
/// this hold; this asserts it from the outside.)
fn assert_head_typechecks(tmp: &TempDir, branch: &str) {
    let store = lex_store::Store::open(tmp.path()).unwrap();
    let head = store.branch_head(branch).unwrap();
    let stages: Vec<lex_ast::Stage> = head.values().map(|id| store.get_ast(id).unwrap()).collect();
    assert!(!stages.is_empty());
    lex_types::check_program(&stages)
        .unwrap_or_else(|e| panic!("branch head no longer type-checks: {e:?}"));
}

fn set_current_branch(tmp: &TempDir, name: &str, create_from: Option<&str>) {
    let store = lex_store::Store::open(tmp.path()).unwrap();
    if let Some(from) = create_from {
        store.create_branch(name, from).unwrap();
    }
    store.set_current_branch(name).unwrap();
}

// ── the four transforms land typed ops with the intent attached ────────────

#[test]
fn replace_match_arm_lands_a_typed_op_with_the_supplied_intent() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let before = head_of(&srv, "main");
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform(
            "main",
            intent("pick 42 for zero", "s-arm"),
            replace_arm(&stage, json!({"kind": "Int", "value": 42})),
        ),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["branch"], "main");
    assert_eq!(v["kind"], "replace_match_arm");
    let op_id = v["op_id"].as_str().unwrap().to_string();
    assert_eq!(v["op_ids"], json!([op_id]));

    // The head advanced, to exactly this op, whose parent was the old head.
    assert_ne!(head_of(&srv, "main"), before);
    assert_eq!(head_of(&srv, "main"), json!(op_id));
    assert_eq!(v["new_head"], json!(op_id));
    assert_eq!(v["prev_head"], before);
    let rec = op_record(&tmp, &op_id);
    assert_eq!(rec.op.parents, vec![before.as_str().unwrap().to_string()]);
    assert!(matches!(rec.op.kind, lex_vcs::OperationKind::ReplaceMatchArm { arm_index: 0, .. }));

    // Read back over the op-log HTTP endpoints too: the op is in the delta
    // since the old head, and its intent is fetchable by id.
    let (s, b) = http(
        &srv.addr,
        "GET",
        &format!("/v1/ops/since?branch=main&after={}", before.as_str().unwrap()),
        "",
    );
    assert_eq!(s, 200, "{b}");
    let delta: Value = serde_json::from_str(&b).unwrap();
    assert_eq!(delta.as_array().unwrap().len(), 1, "{delta}");
    assert_eq!(delta[0]["op_id"], json!(op_id));
    let intent_id = delta[0]["intent_id"].as_str().expect("op carries intent_id");
    assert_eq!(v["intent"]["intent_id"], json!(intent_id));
    assert_eq!(v["intent"]["unattributed"], false);
    let (s, fetched) = post(&srv, "/v1/intents/fetch", &json!({"ids": [intent_id]}));
    assert_eq!(s, 200);
    assert_eq!(fetched["intents"][0]["prompt"], "pick 42 for zero");
    assert_eq!(fetched["intents"][0]["session_id"], "s-arm");
    assert_eq!(fetched["intents"][0]["model"]["provider"], "test");

    // The new stage has the new arm, and the head still type-checks.
    let new_stage = v["new_stage_id"].as_str().unwrap();
    assert_ne!(new_stage, stage);
    assert_head_typechecks(&tmp, "main");
}

#[test]
fn rename_local_lands_a_typed_op_with_the_supplied_intent() {
    let (srv, tmp, stage) = hub_with(ADD_TWO_SRC);
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform(
            "main",
            intent("clearer name", "s-rename"),
            json!({"kind": "rename_local", "from_stage_id": stage, "let_node": "n_0.2", "new_name": "y"}),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let op_id = v["op_id"].as_str().unwrap();
    let rec = op_record(&tmp, op_id);
    match &rec.op.kind {
        lex_vcs::OperationKind::RenameLocal { old_name, new_name, .. } => {
            assert_eq!((old_name.as_str(), new_name.as_str()), ("x", "y"));
        }
        k => panic!("expected RenameLocal, got {k:?}"),
    }
    assert_eq!(intent_of(&tmp, op_id).prompt, "clearer name");
    assert_eq!(head_of(&srv, "main"), json!(op_id));
    assert_head_typechecks(&tmp, "main");
}

#[test]
fn inline_let_lands_a_typed_op_with_the_supplied_intent() {
    let (srv, tmp, stage) = hub_with(ADD_TWO_SRC);
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform(
            "main",
            intent("drop the temp", "s-inline"),
            json!({"kind": "inline_let", "from_stage_id": stage, "let_node": "n_0.2"}),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let op_id = v["op_id"].as_str().unwrap();
    let rec = op_record(&tmp, op_id);
    match &rec.op.kind {
        lex_vcs::OperationKind::InlineLet { binding_name, .. } => assert_eq!(binding_name, "x"),
        k => panic!("expected InlineLet, got {k:?}"),
    }
    let i = intent_of(&tmp, op_id);
    assert_eq!((i.prompt.as_str(), i.session_id.as_str()), ("drop the temp", "s-inline"));
    assert_eq!(head_of(&srv, "main"), json!(op_id));
    assert_head_typechecks(&tmp, "main");
}

fn double_n_spec(return_ty: &str) -> Value {
    json!({
        "name": "double_n",
        "params": [{"name": "n", "type": {"node": "Named", "name": "Int", "args": []}}],
        "return_type": {"node": "Named", "name": return_ty, "args": []},
    })
}

#[test]
fn extract_function_lands_both_typed_ops_under_the_supplied_intent() {
    let (srv, tmp, stage) = hub_with(COMBINE_SRC);
    let before = head_of(&srv, "main");
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform(
            "main",
            intent("factor out doubling", "s-extract"),
            json!({"kind": "extract_function", "from_stage_id": stage,
                   "expr_node": "n_0.3.0", "spec": double_n_spec("Int")}),
        ),
    );
    assert_eq!(s, 200, "{v}");
    let ops: Vec<String> =
        v["op_ids"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    assert_eq!(ops.len(), 2, "AddFunction + ModifyBody: {v}");
    assert_eq!(v["op_id"], json!(ops[1]), "op_id is the last op == the new head");
    assert_eq!(head_of(&srv, "main"), json!(ops[1]));
    assert!(matches!(op_record(&tmp, &ops[0]).op.kind, lex_vcs::OperationKind::AddFunction { .. }));
    assert!(matches!(op_record(&tmp, &ops[1]).op.kind, lex_vcs::OperationKind::ModifyBody { .. }));
    // Both ops carry the SUPPLIED intent (not the synthetic extract intent).
    for op in &ops {
        assert_eq!(intent_of(&tmp, op).prompt, "factor out doubling");
    }
    assert_eq!(
        op_record(&tmp, &ops[0]).op.intent_id,
        op_record(&tmp, &ops[1]).op.intent_id
    );
    assert_eq!(op_record(&tmp, &ops[0]).op.parents, vec![before.as_str().unwrap().to_string()]);
    // The extracted fn is reported and is on the head alongside the source.
    assert!(v["extracted"]["sig_id"].is_string(), "{v}");
    let store = lex_store::Store::open(tmp.path()).unwrap();
    let head = store.branch_head("main").unwrap();
    assert_eq!(head.len(), 2);
    assert_eq!(head.get(v["extracted"]["sig_id"].as_str().unwrap()).map(String::as_str),
               v["extracted"]["stage_id"].as_str());
    assert_head_typechecks(&tmp, "main");
}

// ── the gate: a breaking transform leaves no trace ─────────────────────────

#[test]
fn a_transform_that_breaks_the_typecheck_is_422_and_leaves_no_trace() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let head = head_of(&srv, "main");
    let (ops, intents) = (op_count(&tmp), intent_count(&tmp));

    // arm 0 becomes a Str in an fn that returns Int.
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform(
            "main",
            intent("break it", "s-bad"),
            replace_arm(&stage, json!({"kind": "Str", "value": "oops"})),
        ),
    );
    assert_eq!(s, 422, "{v}");
    assert!(
        !v["detail"]["errors"].as_array().expect("diagnostics under detail.errors").is_empty(),
        "{v}"
    );
    assert!(v.to_string().contains("type_mismatch"), "structured TypeError expected: {v}");
    assert_eq!(head_of(&srv, "main"), head, "head must not move");
    assert_eq!(op_count(&tmp), ops, "no op may be written by a refused transform");
    assert_eq!(intent_count(&tmp), intents, "no intent may be written by a refused transform");
    assert_head_typechecks(&tmp, "main");
}

/// Refuse the extraction `spec` and prove nothing of it landed.
fn assert_extraction_refused_whole(spec: Value, why: &str) {
    let (srv, tmp, stage) = hub_with(COMBINE_SRC);
    let head = head_of(&srv, "main");
    let (ops, intents) = (op_count(&tmp), intent_count(&tmp));
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform(
            "main",
            intent("bad extraction", "s-bad-x"),
            json!({"kind": "extract_function", "from_stage_id": stage,
                   "expr_node": "n_0.3.0", "spec": spec}),
        ),
    );
    assert_eq!(s, 422, "{why}: {v}");
    assert!(!v["detail"]["errors"].as_array().unwrap().is_empty(), "{why}: {v}");
    assert_eq!(head_of(&srv, "main"), head, "{why}");
    assert_eq!(op_count(&tmp), ops, "{why}: not even the AddFunction half may land");
    assert_eq!(intent_count(&tmp), intents, "{why}");
    let store = lex_store::Store::open(tmp.path()).unwrap();
    assert_eq!(store.branch_head("main").unwrap().len(), 1, "{why}: no stranded extracted fn");
}

#[test]
fn an_extraction_that_breaks_the_typecheck_is_422_and_atomic() {
    // 1. The extracted fn is declared to return Str, so ITS OWN body is ill-typed:
    //    refused at the first op of the pair.
    assert_extraction_refused_whole(double_n_spec("Str"), "ill-typed extracted fn");
    // 2. The extracted fn is fine on its own (an over-declared `io` effect is
    //    legal) but the pure source that now calls it is not: only the SECOND
    //    op of the pair fails. The pair must still be refused whole — this is
    //    the case the pre-flight (and, behind it, the rollback) exists for.
    let mut effectful = double_n_spec("Int");
    effectful["effects"] = json!([{"name": "io"}]);
    assert_extraction_refused_whole(effectful, "pure caller of an effectful extraction");
}

// ── refusals ────────────────────────────────────────────────────────────────

#[test]
fn unknown_branch_404_missing_branch_400_and_other_malformed_bodies_400() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let head = head_of(&srv, "main");
    let ops = op_count(&tmp);
    let t = replace_arm(&stage, json!({"kind": "Int", "value": 7}));

    let (s, v) = post(&srv, "/v1/transform", &transform("no-such-branch", intent("p", "s"), t.clone()));
    assert_eq!(s, 404, "{v}");
    assert!(v["error"].as_str().unwrap().contains("no-such-branch"), "{v}");

    // `branch` is REQUIRED: absent, null and empty are all 400, never "the current branch".
    for body in [
        json!({"intent": intent("p", "s"), "transform": t}),
        json!({"branch": null, "transform": t}),
        json!({"branch": "", "transform": t}),
    ] {
        let (s, v) = post(&srv, "/v1/transform", &body);
        assert_eq!(s, 400, "{body} -> {v}");
        assert!(v["error"].as_str().unwrap().contains("branch"), "{v}");
    }
    // A path-shaped branch name is just an unknown branch, not a file access.
    let (s, _) = post(&srv, "/v1/transform", &transform("../main", intent("p", "s"), t.clone()));
    assert_eq!(s, 404);

    // Not JSON / unknown kind / typo'd param / blank prompt.
    let (s, _) = http(&srv.addr, "POST", "/v1/transform", "not json");
    assert_eq!(s, 400);
    let (s, v) = post(&srv, "/v1/transform", &transform("main", intent("p", "s"), json!({"kind": "frobnicate"})));
    assert_eq!(s, 400, "{v}");
    let mut typo = t.clone();
    typo["arm_idx"] = json!(0);
    let (s, v) = post(&srv, "/v1/transform", &transform("main", intent("p", "s"), typo));
    assert_eq!(s, 400, "unknown fields are refused, not ignored: {v}");
    let (s, v) = post(&srv, "/v1/transform", &transform("main", json!({"prompt": "  "}), t.clone()));
    assert_eq!(s, 400, "{v}");
    let (s, v) = post(&srv, "/v1/transform", &transform("main", json!({"bogus": 1}), t));
    assert_eq!(s, 400, "{v}");

    assert_eq!(head_of(&srv, "main"), head);
    assert_eq!(op_count(&tmp), ops);
}

#[test]
fn unknown_stage_and_unaddressable_node_are_4xx_with_the_stores_message() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let head = head_of(&srv, "main");
    let ops = op_count(&tmp);

    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform("main", intent("p", "s"), replace_arm(&"0".repeat(64), json!({"kind": "Int", "value": 1}))),
    );
    assert_eq!(s, 404, "{v}");
    assert!(v["error"].as_str().unwrap().contains("unknown stage_id"), "{v}");

    // A node that does not exist, then an arm index out of range.
    let mut bad_node = replace_arm(&stage, json!({"kind": "Int", "value": 1}));
    bad_node["match_node"] = json!("n_0.99.99");
    let (s, v) = post(&srv, "/v1/transform", &transform("main", intent("p", "s"), bad_node));
    assert_eq!(s, 422, "{v}");
    assert!(v["error"].as_str().unwrap().contains("unknown node id"), "{v}");
    let mut bad_arm = replace_arm(&stage, json!({"kind": "Int", "value": 1}));
    bad_arm["arm_index"] = json!(9);
    let (s, v) = post(&srv, "/v1/transform", &transform("main", intent("p", "s"), bad_arm));
    assert_eq!(s, 422, "{v}");
    assert!(v["error"].as_str().unwrap().contains("out of range"), "{v}");

    assert_eq!(head_of(&srv, "main"), head);
    assert_eq!(op_count(&tmp), ops);
}

#[test]
fn a_stale_from_stage_id_is_409_not_silently_transformed() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform("main", intent("first", "s1"), replace_arm(&stage, json!({"kind": "Int", "value": 42}))),
    );
    assert_eq!(s, 200, "{v}");
    let head = head_of(&srv, "main");
    let ops = op_count(&tmp);
    // `stage` is no longer what the head binds to this function.
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform("main", intent("second", "s2"), replace_arm(&stage, json!({"kind": "Int", "value": 43}))),
    );
    assert_eq!(s, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("stale from_stage_id"), "{v}");
    assert_eq!(head_of(&srv, "main"), head);
    assert_eq!(op_count(&tmp), ops);
}

// ── the request's branch is honoured, not the server's global one ──────────

/// `main` and `feature` hold the same program; the server's global current
/// branch is `feature`. Returns the stage id and both heads.
fn two_branches() -> (Server, TempDir, String, Value, Value) {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    set_current_branch(&tmp, "feature", Some("main"));
    let (m, f) = (head_of(&srv, "main"), head_of(&srv, "feature"));
    (srv, tmp, stage, m, f)
}

#[test]
fn transform_writes_to_the_named_branch_even_when_current_is_another() {
    let (srv, tmp, stage, main_before, feature_before) = two_branches();
    assert_eq!(lex_store::Store::open(tmp.path()).unwrap().current_branch(), "feature");
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform("main", intent("target main", "s"), replace_arm(&stage, json!({"kind": "Int", "value": 42}))),
    );
    assert_eq!(s, 200, "{v}");
    assert_ne!(head_of(&srv, "main"), main_before, "the named branch must advance");
    assert_eq!(head_of(&srv, "feature"), feature_before, "the global current branch must NOT be touched");
    assert_eq!(v["branch"], "main");

    // And the reverse: naming `feature` writes there (fresh from_stage_id: feature is untouched).
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &transform("feature", intent("target feature", "s"), replace_arm(&stage, json!({"kind": "Int", "value": 9}))),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(head_of(&srv, "feature"), json!(v["op_id"]));
}

#[test]
fn patch_honours_an_explicit_branch_even_when_current_is_another() {
    let (srv, _tmp, stage, main_before, feature_before) = two_branches();
    let body = json!({
        "stage_id": stage,
        "patch": {"op": "replace", "target": "n_0.2",
                  "with": {"node": "Literal", "value": {"kind": "Int", "value": 77}}},
        "branch": "main",
    });
    let (s, v) = post(&srv, "/v1/patch", &body);
    assert_eq!(s, 200, "{v}");
    assert_ne!(head_of(&srv, "main"), main_before, "the named branch must advance");
    assert_eq!(head_of(&srv, "feature"), feature_before, "the global current branch must NOT be touched");
    assert_eq!(v["branch"], "main");
    assert_eq!(head_of(&srv, "main"), v["op_id"]);
}

#[test]
fn patch_without_branch_still_writes_to_the_current_branch_as_before() {
    let (srv, _tmp, stage, main_before, feature_before) = two_branches();
    let body = json!({
        "stage_id": stage,
        "patch": {"op": "replace", "target": "n_0.2",
                  "with": {"node": "Literal", "value": {"kind": "Int", "value": 77}}},
    });
    let (s, v) = post(&srv, "/v1/patch", &body);
    assert_eq!(s, 200, "{v}");
    assert_eq!(head_of(&srv, "main"), main_before);
    assert_ne!(head_of(&srv, "feature"), feature_before, "legacy: the current branch");
    // Byte-for-byte legacy response: no new keys unless the caller opted in.
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["new_stage_id", "old_stage_id", "op_id", "sig_id", "status"]);
}

#[test]
fn patch_unknown_branch_is_404_and_writes_nothing() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let head = head_of(&srv, "main");
    let ops = op_count(&tmp);
    let body = json!({
        "stage_id": stage,
        "patch": {"op": "replace", "target": "n_0.2",
                  "with": {"node": "Literal", "value": {"kind": "Int", "value": 77}}},
        "branch": "nope",
    });
    let (s, v) = post(&srv, "/v1/patch", &body);
    assert_eq!(s, 404, "{v}");
    assert_eq!(head_of(&srv, "main"), head);
    assert_eq!(op_count(&tmp), ops);
}

#[test]
fn patch_with_an_intent_attributes_the_op_and_without_one_records_none() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let body = json!({
        "stage_id": stage,
        "patch": {"op": "replace", "target": "n_0.2",
                  "with": {"node": "Literal", "value": {"kind": "Int", "value": 77}}},
        "branch": "main",
        "intent": intent("patch with a reason", "s-patch"),
    });
    let (s, v) = post(&srv, "/v1/patch", &body);
    assert_eq!(s, 200, "{v}");
    let op_id = v["op_id"].as_str().unwrap();
    assert_eq!(intent_of(&tmp, op_id).prompt, "patch with a reason");
    assert_eq!(v["intent_id"], json!(intent_of(&tmp, op_id).intent_id));

    // Intent-less legacy call: still no intent on the op.
    let new_stage = v["new_stage_id"].as_str().unwrap();
    let body2 = json!({
        "stage_id": new_stage,
        "patch": {"op": "replace", "target": "n_0.2",
                  "with": {"node": "Literal", "value": {"kind": "Int", "value": 78}}},
    });
    let (s, v2) = post(&srv, "/v1/patch", &body2);
    assert_eq!(s, 200, "{v2}");
    assert!(op_record(&tmp, v2["op_id"].as_str().unwrap()).op.intent_id.is_none());
}

#[test]
fn a_refused_patch_with_an_intent_leaves_no_intent_behind() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let (ops, intents) = (op_count(&tmp), intent_count(&tmp));
    let body = json!({
        "stage_id": stage,
        "patch": {"op": "replace", "target": "n_0.2",
                  "with": {"node": "Literal", "value": {"kind": "Str", "value": "oops"}}},
        "branch": "main",
        "intent": intent("bad", "s"),
    });
    let (s, v) = post(&srv, "/v1/patch", &body);
    assert_eq!(s, 422, "{v}");
    assert_eq!(op_count(&tmp), ops);
    assert_eq!(intent_count(&tmp), intents);
}

// ── determinism and the unattributed default ───────────────────────────────

#[test]
fn same_transform_and_intent_on_identical_stores_yield_the_same_op_id() {
    let run = |prompt: &str, session: &str| {
        let (srv, _tmp, stage) = hub_with(PICK_SRC);
        let (s, v) = post(
            &srv,
            "/v1/transform",
            &transform("main", intent(prompt, session), replace_arm(&stage, json!({"kind": "Int", "value": 42}))),
        );
        assert_eq!(s, 200, "{v}");
        (v["op_id"].as_str().unwrap().to_string(), v["intent"]["intent_id"].as_str().unwrap().to_string())
    };
    let (a, ia) = run("same reason", "pinned");
    let (b, ib) = run("same reason", "pinned");
    assert_eq!(a, b, "same input + same intent => same OpId");
    assert_eq!(ia, ib);
    // Negative controls: the intent is part of the identity.
    assert_ne!(run("another reason", "pinned").0, a);
    assert_ne!(run("same reason", "another-session").0, a);
}

#[test]
fn an_absent_intent_is_recorded_as_explicitly_unattributed() {
    let (srv, tmp, stage) = hub_with(PICK_SRC);
    let (s, v) = post(
        &srv,
        "/v1/transform",
        &json!({"branch": "main", "transform": replace_arm(&stage, json!({"kind": "Int", "value": 5}))}),
    );
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["intent"]["unattributed"], true);
    let i = intent_of(&tmp, v["op_id"].as_str().unwrap());
    assert_eq!(i.prompt, lex_api::transform_http::UNATTRIBUTED_PROMPT);
    // The session is echoed, not silently invented.
    assert_eq!(v["intent"]["session_id"], json!(i.session_id));
    assert!(i.session_id.starts_with("http-"), "{}", i.session_id);
}
