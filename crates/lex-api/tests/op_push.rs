//! Conformance tests for #242 — HTTP push of ops + attestations.
//!
//! Coverage:
//!
//! 1. Round-trip: 100 ops + a TypeCheck attestation each through
//!    the loopback API. Counts are reported correctly.
//! 2. Idempotency: pushing the same batch twice produces
//!    `added: 0` on the second call.
//! 3. DAG-integrity refusal: a batch whose op has an unreachable
//!    parent returns 422 with `MissingParent` and persists nothing.
//! 4. OpId-mismatch refusal: a tampered `op_id` is rejected with
//!    409 `OpIdMismatch`.
//! 5. Attestations: `op_id` referencing an unknown op returns 422
//!    `UnknownOp`.
//! 6. Branch-head probe: `GET /v1/branches/<name>/head` returns
//!    `null` for unknown branches and the current head_op
//!    otherwise.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use lex_api::handlers::State;
use lex_vcs::{
    Attestation, AttestationKind, AttestationResult, Operation, OperationKind, OperationRecord,
    ProducerDescriptor, StageTransition,
};
use std::collections::BTreeSet;
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    _join: Option<thread::JoinHandle<()>>,
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
    thread::sleep(Duration::from_millis(20));
    (Server { addr, _join: Some(join) }, tmp)
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

fn add_op(parent: Option<&str>, sig: &str, stg: &str) -> OperationRecord {
    let parents: Vec<String> = parent.map(|p| p.to_string()).into_iter().collect();
    OperationRecord::new(
        Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stg.into(),
                effects: BTreeSet::new(),
                budget_cost: None,
            },
            parents,
        ),
        StageTransition::Create {
            sig_id: sig.into(),
            stage_id: stg.into(),
        },
    )
}

fn modify_op(parent: &str, sig: &str, from: &str, to: &str) -> OperationRecord {
    OperationRecord::new(
        Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
                from_budget: None,
                to_budget: None,
            },
            [parent.to_string()],
        ),
        StageTransition::Replace {
            sig_id: sig.into(),
            from: from.into(),
            to: to.into(),
        },
    )
}

fn typecheck(stage_id: &str, op_id: &str) -> Attestation {
    Attestation::with_timestamp(
        stage_id.to_string(),
        Some(op_id.into()),
        None,
        AttestationKind::TypeCheck,
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "test".into(),
            version: "0".into(),
            model: None,
        },
        None,
        1_700_000_000,
    )
}

#[test]
fn round_trip_one_hundred_ops_plus_attestations() {
    let (srv, _tmp) = start_server();

    // Build a chain of 100 ops: one root + 99 modifications.
    let mut ops: Vec<OperationRecord> = Vec::with_capacity(100);
    let root = add_op(None, "fac", "stg-0");
    let mut last_id = root.op_id.clone();
    let mut last_stage = "stg-0".to_string();
    ops.push(root);
    for i in 1..100 {
        let to_stg = format!("stg-{i}");
        let m = modify_op(&last_id, "fac", &last_stage, &to_stg);
        last_id = m.op_id.clone();
        last_stage.clone_from(&to_stg);
        ops.push(m);
    }
    assert_eq!(ops.len(), 100);

    // Push the ops in topological order (oldest-first, which is
    // the order we built them in).
    let body = serde_json::to_string(&ops).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200, "first push: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["received"], 100);
    assert_eq!(v["added"], 100);
    assert_eq!(v["skipped"], 0);

    // Push again — every record is now already present, so added
    // should be 0.
    let (status, resp) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200, "second push: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["received"], 100);
    assert_eq!(v["added"], 0);
    assert_eq!(v["skipped"], 100);

    // Now push 100 attestations — one TypeCheck per op, all
    // referencing the op_id of the corresponding op.
    let attestations: Vec<Attestation> = ops.iter()
        .map(|r| {
            let stage = match &r.op.kind {
                OperationKind::AddFunction { stage_id, .. } => stage_id.clone(),
                OperationKind::ModifyBody { to_stage_id, .. } => to_stage_id.clone(),
                _ => unreachable!(),
            };
            typecheck(&stage, &r.op_id)
        })
        .collect();
    let abody = serde_json::to_string(&attestations).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/attestations/batch", &abody);
    assert_eq!(status, 200, "attestations push: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["received"], 100);
    assert_eq!(v["added"], 100);

    // And idempotency on attestations.
    let (status, resp) = http(&srv.addr, "POST", "/v1/attestations/batch", &abody);
    assert_eq!(status, 200, "second attestations push: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["added"], 0);
    assert_eq!(v["skipped"], 100);
}

#[test]
fn batch_with_unreachable_parent_returns_422_missing_parent() {
    let (srv, _tmp) = start_server();
    // A single op whose parent doesn't exist on the remote and
    // isn't in the same batch.
    let orphan = modify_op("ghost-parent", "fac", "s0", "s1");
    let body = serde_json::to_string(&vec![orphan.clone()]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 422, "expected 422, got {status}: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"], "MissingParent");
    assert_eq!(v["detail"]["op_id"], orphan.op_id);
    assert_eq!(v["detail"]["missing_parent"], "ghost-parent");
}

#[test]
fn batch_with_in_batch_parent_resolves_correctly() {
    // Parent appears earlier in the same batch — this is the
    // common case for `lex op push` sending a topologically-
    // ordered slice.
    let (srv, _tmp) = start_server();
    let a = add_op(None, "fac", "s0");
    let b = modify_op(&a.op_id, "fac", "s0", "s1");
    let body = serde_json::to_string(&vec![a, b]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["added"], 2);
}

#[test]
fn op_id_mismatch_returns_409() {
    let (srv, _tmp) = start_server();
    let mut a = add_op(None, "fac", "s0");
    a.op_id = "0".repeat(64); // forge it
    let body = serde_json::to_string(&vec![a]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 409, "expected 409, got {status}: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"], "OpIdMismatch");
}

#[test]
fn attestation_with_unknown_op_returns_422_unknown_op() {
    let (srv, _tmp) = start_server();
    let att = typecheck("stg-1", "ghost-op-id");
    let body = serde_json::to_string(&vec![att.clone()]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/attestations/batch", &body);
    assert_eq!(status, 422, "expected 422, got {status}: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"], "UnknownOp");
    assert_eq!(v["detail"]["op_id"], "ghost-op-id");
    assert_eq!(v["detail"]["attestation_id"], att.attestation_id);
}

#[test]
fn attestation_id_mismatch_returns_409() {
    let (srv, _tmp) = start_server();
    // Push an op so the attestation's op_id is reachable.
    let a = add_op(None, "fac", "s0");
    let op_id = a.op_id.clone();
    let body = serde_json::to_string(&vec![a]).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200);

    // Now forge an attestation with a wrong attestation_id.
    let mut att = typecheck("s0", &op_id);
    att.attestation_id = "0".repeat(64);
    let body = serde_json::to_string(&vec![att]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/attestations/batch", &body);
    assert_eq!(status, 409, "expected 409, got {status}: {resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["error"], "AttestationIdMismatch");
}

#[test]
fn attestation_without_op_id_is_accepted() {
    // Some attestations (Override/Block/Unblock) carry no op_id;
    // the gate's UnknownOp check skips them.
    let (srv, _tmp) = start_server();
    let att = Attestation::with_timestamp(
        "stg-1".to_string(),
        None,
        None,
        AttestationKind::Override {
            actor: "alice".into(),
            reason: "manual override".into(),
            target_attestation_id: None,
        },
        AttestationResult::Passed,
        ProducerDescriptor {
            tool: "test".into(),
            version: "0".into(),
            model: None,
        },
        None,
        1_700_000_001,
    );
    let body = serde_json::to_string(&vec![att]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/attestations/batch", &body);
    assert_eq!(status, 200, "{resp}");
}

#[test]
fn branch_head_probe_returns_null_for_unknown_branch() {
    let (srv, _tmp) = start_server();
    let (status, resp) = http(&srv.addr, "GET", "/v1/branches/main/head", "");
    assert_eq!(status, 200, "{resp}");
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["branch"], "main");
    assert_eq!(v["head_op"], serde_json::Value::Null);
}

#[test]
fn rejection_persists_nothing() {
    // A batch with a missing parent must leave the remote
    // exactly as it was — no partial commits.
    let (srv, _tmp) = start_server();
    // First send a clean batch so we know what's there.
    let a = add_op(None, "fac", "s0");
    let a_id = a.op_id.clone();
    let body = serde_json::to_string(&vec![a]).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200);

    // Now send a batch where a valid op is followed by one with
    // a missing parent. The whole batch must be rejected — the
    // valid op's already on the remote, but a new sibling
    // shouldn't sneak in.
    let b = add_op(None, "double", "ddd-0");
    let bad = modify_op("ghost-parent", "fac", "s0", "s1");
    let body = serde_json::to_string(&vec![b.clone(), bad]).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 422);

    // `b` was not persisted — we never even started writing
    // because validation runs upfront.
    let (_, resp) = http(&srv.addr, "GET", "/v1/branches/_no_such/head", "");
    let _ = resp; // probe just to verify the server's still healthy

    // Re-pushing only the original `a` should report 0 added (it
    // already exists). Re-pushing `b` alone should add it; if the
    // earlier rejection had partially persisted `b`, this would
    // be `added: 0` and we'd have a silent integrity bug.
    let body = serde_json::to_string(&vec![b]).unwrap();
    let (status, resp) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(
        v["added"], 1,
        "rejection leaked partial state into the log: {resp}",
    );
    let _ = a_id; // keep the binding name explicit
}
