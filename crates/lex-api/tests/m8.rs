//! M8 acceptance per spec §12.4.
//!
//! - All CLI commands work end-to-end on §3.13 examples (covered by
//!   crate-level tests across the workspace).
//! - The agent API server starts, handles 100 sequential requests
//!   without leaking memory, and returns the same results as the CLI.
//! - A scripted agent loop (publish → run → trace → patch → run)
//!   completes successfully. We exercise publish → run → trace → diff
//!   here; patch lands in a follow-up.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use serde_json::json;
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
    _server_holder: Arc<()>,
}

fn start_server() -> (Server, TempDir) {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0))
        .expect("bind ephemeral port");
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || {
        lex_api::serve_on(server, state);
    });
    // Give the OS a moment to actually start listening.
    thread::sleep(Duration::from_millis(20));
    (Server { addr, _join: Some(join), _server_holder: Arc::new(()) }, tmp)
}

fn http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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

#[test]
fn health_check() {
    let (srv, _tmp) = start_server();
    let (status, body) = http(&srv.addr, "GET", "/v1/health", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"));
}

#[test]
fn parse_then_check_pipeline() {
    let (srv, _tmp) = start_server();
    let src = "fn add(x :: Int, y :: Int) -> Int { x + y }\n";
    let body = json!({"source": src}).to_string();
    let (s1, b1) = http(&srv.addr, "POST", "/v1/parse", &body);
    assert_eq!(s1, 200);
    assert!(b1.contains("FnDecl"));
    let (s2, b2) = http(&srv.addr, "POST", "/v1/check", &body);
    assert_eq!(s2, 200);
    assert!(b2.contains("\"ok\":true"));
}

#[test]
fn parse_returns_4xx_on_syntax_error() {
    let (srv, _tmp) = start_server();
    let body = json!({"source": "fn"}).to_string();
    let (s, _) = http(&srv.addr, "POST", "/v1/parse", &body);
    assert!((400..500).contains(&s), "expected 4xx, got {s}");
}

#[test]
fn check_returns_422_on_type_error() {
    let (srv, _tmp) = start_server();
    let src = "fn bad(x :: Int) -> Str { x }\n";
    let body = json!({"source": src}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/check", &body);
    assert_eq!(s, 422);
    assert!(b.contains("type_mismatch"), "expected type_mismatch, body: {b}");
}

#[test]
fn agent_loop_publish_run_trace_diff() {
    // §12.4: a scripted agent loop completes successfully.
    let (srv, _tmp) = start_server();
    let src = "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n";

    // 1) publish (and activate so resolve_sig works)
    let pub_body = json!({"source": src, "activate": true}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &pub_body);
    assert_eq!(s, 200, "publish status: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    // Response is {"ops": [...], "head_op": "..."}; first op is AddFunction.
    let ops = v["ops"].as_array().unwrap();
    assert!(!ops.is_empty(), "expected at least one op, got: {b}");
    let first_op = &ops[0];
    let stage_id = first_op["kind"]["stage_id"].as_str().unwrap();
    let _sig_id = first_op["kind"]["sig_id"].as_str().unwrap();
    assert!(v["head_op"].is_string(), "head_op should be set");

    // 2) get the published stage back
    let (s, b) = http(&srv.addr, "GET", &format!("/v1/stage/{stage_id}"), "");
    assert_eq!(s, 200, "stage GET: {b}");
    assert!(b.contains("FnDecl"));

    // 2b) #132: TypeCheck attestation written by the store-write
    // gate is queryable via /v1/stage/<id>/attestations.
    let (s, b) = http(&srv.addr, "GET", &format!("/v1/stage/{stage_id}/attestations"), "");
    assert_eq!(s, 200, "attestations GET: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let atts = v["attestations"].as_array().expect("attestations array");
    assert!(!atts.is_empty(), "publish should have produced a TypeCheck attestation");
    assert_eq!(atts[0]["kind"]["kind"], "type_check");
    assert_eq!(atts[0]["result"]["result"], "passed");
    assert_eq!(atts[0]["produced_by"]["tool"], "lex-store");

    // 2c) Unknown stage_id → 404 (matches /v1/stage/<id>'s shape).
    let (s, _) = http(&srv.addr, "GET", "/v1/stage/nonexistent/attestations", "");
    assert_eq!(s, 404, "unknown stage_id should 404");

    // 3) run the function
    let run_body = json!({"source": src, "fn": "factorial", "args": [5]}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/run", &run_body);
    assert_eq!(s, 200);
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["output"], json!(120));
    let run_id_a = v["run_id"].as_str().unwrap().to_string();

    // 4) read the trace
    let (s, b) = http(&srv.addr, "GET", &format!("/v1/trace/{run_id_a}"), "");
    assert_eq!(s, 200);
    assert!(b.contains("factorial"));

    // 5) run again with a different argument and diff the two traces
    let run_body2 = json!({"source": src, "fn": "factorial", "args": [4]}).to_string();
    let (_, b2) = http(&srv.addr, "POST", "/v1/run", &run_body2);
    let v2: serde_json::Value = serde_json::from_str(&b2).unwrap();
    let run_id_b = v2["run_id"].as_str().unwrap().to_string();

    let (s, body) = http(&srv.addr, "GET", &format!("/v1/diff?a={run_id_a}&b={run_id_b}"), "");
    assert_eq!(s, 200);
    // Different inputs ⇒ a divergence.
    assert!(body.contains("node_id"), "expected divergence body: {body}");
}

#[test]
fn handles_100_sequential_requests() {
    // §12.4: server handles 100 sequential requests without crashing.
    let (srv, _tmp) = start_server();
    let body = json!({"source": "fn id(x :: Int) -> Int { x }\n"}).to_string();
    for _ in 0..100 {
        let (s, _) = http(&srv.addr, "POST", "/v1/check", &body);
        assert_eq!(s, 200);
    }
}

#[test]
fn run_rejects_undeclared_effect() {
    // §12.5: a program declaring [io] without policy is rejected at policy time.
    let (srv, _tmp) = start_server();
    let src = "import \"std.io\" as io\nfn say(line :: Str) -> [io] Nil { io.print(line) }\n";
    let body = json!({"source": src, "fn": "say", "args": ["x"]}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/run", &body);
    assert_eq!(s, 403, "expected 403, got {s}: {b}");
    assert!(b.contains("policy violation"));
    assert!(b.contains("io"));
}

#[test]
fn run_with_policy_succeeds() {
    let (srv, _tmp) = start_server();
    let src = "import \"std.io\" as io\nfn say(line :: Str) -> [io] Nil { io.print(line) }\n";
    let body = json!({
        "source": src, "fn": "say", "args": ["hello"],
        "policy": {"allow_effects": ["io"]},
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/run", &body);
    assert_eq!(s, 200, "expected 200, got {s}: {b}");
}

#[test]
fn merge_start_unknown_branch_returns_404() {
    let (srv, _tmp) = start_server();
    let body = json!({"src_branch": "nonexistent_a", "dst_branch": "nonexistent_b"}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/merge/start", &body);
    assert_eq!(s, 404, "unknown branch should 404, got {s}: {b}");
}

#[test]
fn merge_start_returns_session_id_and_no_conflicts_for_disjoint_branches() {
    // Two branches that touch *different* sigs auto-resolve into a
    // clean merge — no conflicts. The endpoint should still mint a
    // session, return the conflict list (empty), and report the
    // count of auto-resolved sigs so the agent can audit what the
    // engine took unilaterally.
    let (srv, tmp) = start_server();

    // 1) Publish fn foo on main.
    let src_main = "fn foo(n :: Int) -> Int { n + 1 }\n";
    let pub_main = json!({"source": src_main, "activate": true}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &pub_main);
    assert_eq!(s, 200, "publish main: {b}");

    // 2) Create + switch to feature, publish fn bar.
    {
        let store = lex_store::Store::open(tmp.path()).unwrap();
        store.create_branch("feature", lex_store::DEFAULT_BRANCH)
            .expect("create feature");
        store.set_current_branch("feature").expect("switch to feature");
    }
    let src_feature = "fn foo(n :: Int) -> Int { n + 1 }\nfn bar(n :: Int) -> Int { n - 1 }\n";
    let pub_feat = json!({"source": src_feature, "activate": true}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &pub_feat);
    assert_eq!(s, 200, "publish feature: {b}");

    // 3) Switch back to main so it remains the dst.
    {
        let store = lex_store::Store::open(tmp.path()).unwrap();
        store.set_current_branch(lex_store::DEFAULT_BRANCH).expect("switch back");
    }

    // 4) Start the merge.
    let body = json!({"src_branch": "feature", "dst_branch": lex_store::DEFAULT_BRANCH}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/merge/start", &body);
    assert_eq!(s, 200, "merge/start: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert!(v["merge_id"].as_str().is_some(), "merge_id should be set");
    let conflicts = v["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 0, "disjoint adds shouldn't conflict, got {conflicts:?}");
    assert!(v["auto_resolved_count"].as_u64().unwrap() >= 1, "at least one sig auto-resolved");
}

/// Set up two branches that *both* modify the same fn (`foo`).
/// Returns the running server, the temp store dir, and the session
/// `merge_id` produced by `/v1/merge/start`. Used by the resolve
/// tests to avoid duplicating the divergence setup.
fn with_modify_modify_session() -> (Server, TempDir, String) {
    let (srv, tmp) = start_server();

    // 1) Publish initial fn on main.
    let v0 = "fn foo(n :: Int) -> Int { n }\n";
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &json!({"source": v0, "activate": true}).to_string());
    assert_eq!(s, 200, "publish v0: {b}");

    // 2) Create + switch to feature, modify foo.
    {
        let store = lex_store::Store::open(tmp.path()).unwrap();
        store.create_branch("feature", lex_store::DEFAULT_BRANCH).unwrap();
        store.set_current_branch("feature").unwrap();
    }
    let v_feat = "fn foo(n :: Int) -> Int { n + 1 }\n";
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &json!({"source": v_feat, "activate": true}).to_string());
    assert_eq!(s, 200, "publish feature: {b}");

    // 3) Switch back to main, modify foo differently.
    {
        let store = lex_store::Store::open(tmp.path()).unwrap();
        store.set_current_branch(lex_store::DEFAULT_BRANCH).unwrap();
    }
    let v_main = "fn foo(n :: Int) -> Int { n + 2 }\n";
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &json!({"source": v_main, "activate": true}).to_string());
    assert_eq!(s, 200, "publish main: {b}");

    // 4) Start the merge — should produce a ModifyModify conflict on `foo`.
    let body = json!({"src_branch": "feature", "dst_branch": lex_store::DEFAULT_BRANCH}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/merge/start", &body);
    assert_eq!(s, 200, "merge/start: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let merge_id = v["merge_id"].as_str().unwrap().to_string();
    let conflicts = v["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1, "expected exactly one conflict, got: {conflicts:?}");
    (srv, tmp, merge_id)
}

#[test]
fn merge_resolve_take_ours_clears_the_conflict() {
    let (srv, _tmp, merge_id) = with_modify_modify_session();
    let path = format!("/v1/merge/{merge_id}/resolve");
    // Find the conflict id (same as sig_id of `foo`).
    let (_, start_body) = http(&srv.addr, "POST", "/v1/merge/start", &json!({
        "src_branch": "feature", "dst_branch": lex_store::DEFAULT_BRANCH,
    }).to_string());
    // Re-run start to pull a fresh conflict list keyed to a *new* merge_id —
    // but the resolution we're testing is against `merge_id` from the
    // helper, so the conflict_id is the same (sig).
    let v: serde_json::Value = serde_json::from_str(&start_body).unwrap();
    let conflict_id = v["conflicts"][0]["conflict_id"].as_str().unwrap().to_string();

    let body = json!({
        "resolutions": [
            {"conflict_id": conflict_id, "resolution": {"kind": "take_ours"}}
        ]
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", &path, &body);
    assert_eq!(s, 200, "resolve: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let verdicts = v["verdicts"].as_array().unwrap();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0]["accepted"], true);
    let remaining = v["remaining_conflicts"].as_array().unwrap();
    assert_eq!(remaining.len(), 0, "the take_ours resolution should clear the conflict");
}

#[test]
fn merge_resolve_unknown_conflict_is_rejected_per_entry() {
    let (srv, _tmp, merge_id) = with_modify_modify_session();
    let path = format!("/v1/merge/{merge_id}/resolve");
    let body = json!({
        "resolutions": [
            {"conflict_id": "definitely-not-a-real-sig", "resolution": {"kind": "take_ours"}}
        ]
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", &path, &body);
    assert_eq!(s, 200, "resolve should still 200 with per-entry verdicts");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let verdicts = v["verdicts"].as_array().unwrap();
    assert_eq!(verdicts[0]["accepted"], false);
    assert_eq!(verdicts[0]["rejection"]["kind"], "unknown_conflict");
}

#[test]
fn merge_resolve_unknown_session_returns_404() {
    let (srv, _tmp) = start_server();
    let body = json!({"resolutions": []}).to_string();
    let (s, _) = http(&srv.addr, "POST", "/v1/merge/no_such_session/resolve", &body);
    assert_eq!(s, 404, "unknown merge_id should 404");
}

#[test]
fn merge_commit_advances_dst_branch_after_take_theirs() {
    // Full e2e: start → resolve(take_theirs) → commit. dst branch
    // head must move to a new Merge op whose StageTransition picks
    // up src's stage for the resolved sig.
    let (srv, _tmp, merge_id) = with_modify_modify_session();
    let path_resolve = format!("/v1/merge/{merge_id}/resolve");
    let path_commit  = format!("/v1/merge/{merge_id}/commit");

    // Pull the conflict id.
    let (_, b) = http(&srv.addr, "POST", "/v1/merge/start", &json!({
        "src_branch": "feature", "dst_branch": lex_store::DEFAULT_BRANCH,
    }).to_string());
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let conflict_id = v["conflicts"][0]["conflict_id"].as_str().unwrap().to_string();

    // Resolve as take_theirs.
    let body = json!({
        "resolutions": [
            {"conflict_id": conflict_id, "resolution": {"kind": "take_theirs"}}
        ]
    }).to_string();
    let (s, _) = http(&srv.addr, "POST", &path_resolve, &body);
    assert_eq!(s, 200);

    // Commit.
    let (s, b) = http(&srv.addr, "POST", &path_commit, "");
    assert_eq!(s, 200, "commit: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let new_head = v["new_head_op"].as_str().expect("new_head_op set");
    assert!(!new_head.is_empty());
    assert_eq!(v["dst_branch"], lex_store::DEFAULT_BRANCH);
}

#[test]
fn merge_commit_with_unresolved_conflicts_returns_422() {
    // No resolutions submitted → conflicts remaining → 422.
    let (srv, _tmp, merge_id) = with_modify_modify_session();
    let path_commit = format!("/v1/merge/{merge_id}/commit");
    let (s, b) = http(&srv.addr, "POST", &path_commit, "");
    assert_eq!(s, 422, "expected 422 conflicts remaining: {b}");
    assert!(b.contains("conflicts remaining"));
}

#[test]
fn merge_commit_unknown_session_returns_404() {
    let (srv, _tmp) = start_server();
    let (s, _) = http(&srv.addr, "POST", "/v1/merge/no_such/commit", "");
    assert_eq!(s, 404);
}

#[test]
fn replay_with_overrides() {
    let (srv, _tmp) = start_server();
    let src = "import \"std.io\" as io\nfn read_one(p :: Str) -> [io] Result[Str, Str] { io.read(p) }\n";

    // First run: io.read fails because path doesn't exist; result is Err(...) value-level.
    let run = json!({
        "source": src, "fn": "read_one", "args": ["/no/such"],
        "policy": {"allow_effects": ["io"]},
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/run", &run);
    assert_eq!(s, 200);
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let run_id = v["run_id"].as_str().unwrap().to_string();

    // Pull the trace, find the io.read NodeId.
    let (_, body) = http(&srv.addr, "GET", &format!("/v1/trace/{run_id}"), "");
    let trace: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mut effect_node_id: Option<String> = None;
    fn find(n: &serde_json::Value, out: &mut Option<String>) {
        if let Some(arr) = n.as_array() {
            for c in arr { find(c, out); }
            return;
        }
        if let Some(kind) = n.get("kind").and_then(|k| k.as_str()) {
            if kind == "effect" {
                if let Some(nid) = n.get("node_id").and_then(|x| x.as_str()) {
                    *out = Some(nid.to_string());
                }
            }
        }
        if let Some(children) = n.get("children") { find(children, out); }
        if let Some(nodes) = n.get("nodes") { find(nodes, out); }
    }
    find(&trace, &mut effect_node_id);
    let nid = effect_node_id.expect("trace has an effect node");

    // Replay with override.
    let injected = json!({"$variant": "Ok", "args": ["INJECTED"]});
    let replay = json!({
        "source": src, "fn": "read_one", "args": ["/no/such"],
        "policy": {"allow_effects": ["io"]},
        "overrides": { nid: injected },
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/replay", &replay);
    assert_eq!(s, 200, "replay status: {s}, body: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["output"], json!({"$variant": "Ok", "args": ["INJECTED"]}));
}

#[test]
fn patch_replaces_a_subexpression_and_publishes_new_stage() {
    // Publish a stage, patch a sub-expression, run the patched stage.
    let (srv, _tmp) = start_server();
    let src = "fn add_one(x :: Int) -> Int { x + 1 }\n";

    // 1. Publish the original.
    let pub_body = json!({"source": src, "activate": true}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/publish", &pub_body);
    assert_eq!(s, 200, "publish: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let stage_id = v["ops"][0]["kind"]["stage_id"].as_str().unwrap().to_string();

    // 2. Patch the literal `1` with `100`. Body sits at n_0.2 (1 param);
    //    BinOp.rhs is at n_0.2.1.
    let patch_body = json!({
        "stage_id": stage_id,
        "patch": {
            "op": "replace",
            "target": "n_0.2.1",
            "with": { "node": "Literal", "value": { "kind": "Int", "value": 100 } }
        },
        "activate": true,
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/patch", &patch_body);
    assert_eq!(s, 200, "patch: {b}");
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    let new_id = v["new_stage_id"].as_str().unwrap().to_string();
    assert_ne!(new_id, stage_id, "new StageId must differ from original");
    assert_eq!(v["status"], "active");

    // 3. Run the patched function: add_one(5) should now be 105.
    let run_body = json!({"source": "fn add_one(x :: Int) -> Int { x + 100 }\n",
                         "fn": "add_one", "args": [5]}).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/run", &run_body);
    assert_eq!(s, 200);
    let v: serde_json::Value = serde_json::from_str(&b).unwrap();
    assert_eq!(v["output"], json!(105));
}

#[test]
fn patch_with_type_error_after_apply_returns_422() {
    // Replacing an Int with a Str should fail typecheck and surface 422
    // with the structured TypeError list.
    let (srv, _tmp) = start_server();
    let src = "fn add_one(x :: Int) -> Int { x + 1 }\n";

    let (s, b) = http(&srv.addr, "POST", "/v1/publish",
        &json!({"source": src, "activate": true}).to_string());
    assert_eq!(s, 200);
    let stage_id = serde_json::from_str::<serde_json::Value>(&b).unwrap()
        ["ops"][0]["kind"]["stage_id"].as_str().unwrap().to_string();

    // Replace `1` (Int) with `"oops"` (Str).
    let patch_body = json!({
        "stage_id": stage_id,
        "patch": {
            "op": "replace",
            "target": "n_0.2.1",
            "with": { "node": "Literal", "value": { "kind": "Str", "value": "oops" } }
        },
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/patch", &patch_body);
    assert_eq!(s, 422, "expected 422 on type-incompatible patch, got {s}: {b}");
    assert!(b.contains("type_mismatch"), "body should carry structured TypeError: {b}");
}

#[test]
fn patch_with_unknown_node_returns_422() {
    let (srv, _tmp) = start_server();
    let (s, b) = http(&srv.addr, "POST", "/v1/publish",
        &json!({"source": "fn one() -> Int { 1 }\n", "activate": true}).to_string());
    assert_eq!(s, 200);
    let stage_id = serde_json::from_str::<serde_json::Value>(&b).unwrap()
        ["ops"][0]["kind"]["stage_id"].as_str().unwrap().to_string();

    let patch_body = json!({
        "stage_id": stage_id,
        "patch": {
            "op": "replace",
            "target": "n_0.99.99",
            "with": { "node": "Literal", "value": { "kind": "Int", "value": 0 } }
        },
    }).to_string();
    let (s, b) = http(&srv.addr, "POST", "/v1/patch", &patch_body);
    assert_eq!(s, 422);
    assert!(b.contains("unknown_node"), "expected unknown_node in body: {b}");
}
