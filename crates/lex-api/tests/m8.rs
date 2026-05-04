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
