//! Conformance tests for #260 — HTTP pull of ops + attestations
//! (the inverse of #242's push).
//!
//! Coverage:
//!
//! 1. Pull from empty remote returns `[]`.
//! 2. Pull from a remote that's strictly ahead returns the delta
//!    in oldest-first order.
//! 3. Pull when caller is already at the remote's head returns `[]`.
//! 4. `--limit` chunks the response.
//! 5. Attestation pull respects the `after-op` filter.
//! 6. Attestations with `op_id: None` (Override etc.) ship on
//!    every pull (no cutoff applies).

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
    tmp: TempDir,
    _join: Option<thread::JoinHandle<()>>,
}

fn start_server() -> Server {
    let tmp = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
    let addr: SocketAddr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(tmp.path().to_path_buf()).unwrap());
    let join = thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    Server { addr, tmp, _join: Some(join) }
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

/// Seed the remote by pushing a chain of ops via /v1/ops/batch and
/// then advancing `main`'s head_op via the file directly. (We
/// can't use Store::apply_operation_checked here without a real
/// candidate program; the server-side push endpoint is the
/// supported way to populate a test fixture.)
fn seed_remote_chain(srv: &Server, length: usize) -> Vec<OperationRecord> {
    assert!(length >= 1);
    let mut chain: Vec<OperationRecord> = Vec::new();
    let root = add_op(None, "fac", "stg-0");
    chain.push(root.clone());
    let mut last_id = root.op_id.clone();
    let mut last_stage = "stg-0".to_string();
    for i in 1..length {
        let to_stg = format!("stg-{i}");
        let m = modify_op(&last_id, "fac", &last_stage, &to_stg);
        last_id = m.op_id.clone();
        last_stage.clone_from(&to_stg);
        chain.push(m);
    }
    let body = serde_json::to_string(&chain).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/ops/batch", &body);
    assert_eq!(status, 200);

    // Manually advance main's head_op to the last record. Mirrors
    // the lex op pull's `fast_forward_branch_head` helper.
    let path = srv.tmp.path().join("branches/main.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let value = serde_json::json!({
        "name": "main",
        "parent": serde_json::Value::Null,
        "head_op": chain.last().unwrap().op_id,
        "merges": [],
        "created_at": 0,
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    chain
}

#[test]
fn pull_from_empty_remote_returns_empty_array() {
    let srv = start_server();
    let (status, body) = http(&srv.addr, "GET", "/v1/ops/since?branch=main", "");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v.as_array().map(|a| a.len()), Some(0));
}

#[test]
fn pull_returns_ops_oldest_first() {
    let srv = start_server();
    let chain = seed_remote_chain(&srv, 5);

    let (status, body) = http(&srv.addr, "GET", "/v1/ops/since?branch=main", "");
    assert_eq!(status, 200, "{body}");
    let received: Vec<OperationRecord> = serde_json::from_str(&body).unwrap();
    assert_eq!(received.len(), 5);
    // Oldest-first: first record has no parents, then each subsequent
    // record has the previous one as parent.
    assert!(received[0].op.parents.is_empty(), "first record should be the genesis");
    for i in 1..5 {
        assert_eq!(
            received[i].op.parents, vec![received[i - 1].op_id.clone()],
            "record {i} must reference record {} as parent", i - 1,
        );
    }
    // Match the seeded chain.
    for (got, expected) in received.iter().zip(chain.iter()) {
        assert_eq!(got.op_id, expected.op_id);
    }
}

#[test]
fn pull_with_after_returns_only_new_records() {
    let srv = start_server();
    let chain = seed_remote_chain(&srv, 5);
    let cutoff = &chain[2].op_id;

    let path = format!("/v1/ops/since?branch=main&after={cutoff}");
    let (status, body) = http(&srv.addr, "GET", &path, "");
    assert_eq!(status, 200, "{body}");
    let received: Vec<OperationRecord> = serde_json::from_str(&body).unwrap();
    // Records 3 and 4 are after the cutoff (cutoff itself excluded).
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].op_id, chain[3].op_id);
    assert_eq!(received[1].op_id, chain[4].op_id);
}

#[test]
fn pull_at_remote_head_returns_empty() {
    let srv = start_server();
    let chain = seed_remote_chain(&srv, 3);
    let head = &chain.last().unwrap().op_id;

    let path = format!("/v1/ops/since?branch=main&after={head}");
    let (status, body) = http(&srv.addr, "GET", &path, "");
    assert_eq!(status, 200, "{body}");
    let received: Vec<OperationRecord> = serde_json::from_str(&body).unwrap();
    assert!(received.is_empty(), "caller is at remote head; pull should be a no-op");
}

#[test]
fn pull_with_limit_chunks_the_response() {
    let srv = start_server();
    let chain = seed_remote_chain(&srv, 10);

    let (status, body) = http(&srv.addr, "GET", "/v1/ops/since?branch=main&limit=3", "");
    assert_eq!(status, 200, "{body}");
    let received: Vec<OperationRecord> = serde_json::from_str(&body).unwrap();
    assert_eq!(received.len(), 3);
    // First chunk is the oldest 3 records.
    for i in 0..3 {
        assert_eq!(received[i].op_id, chain[i].op_id);
    }

    // Next chunk via `after`.
    let after = &chain[2].op_id;
    let path = format!("/v1/ops/since?branch=main&after={after}&limit=3");
    let (_, body) = http(&srv.addr, "GET", &path, "");
    let next: Vec<OperationRecord> = serde_json::from_str(&body).unwrap();
    assert_eq!(next.len(), 3);
    assert_eq!(next[0].op_id, chain[3].op_id);
}

#[test]
fn pull_unknown_branch_returns_empty() {
    let srv = start_server();
    let _ = seed_remote_chain(&srv, 2);

    let (status, body) = http(&srv.addr, "GET", "/v1/ops/since?branch=does_not_exist", "");
    assert_eq!(status, 200, "{body}");
    let received: Vec<OperationRecord> = serde_json::from_str(&body).unwrap();
    assert!(received.is_empty());
}

#[test]
fn attestations_pull_respects_after_op_filter() {
    let srv = start_server();
    let chain = seed_remote_chain(&srv, 3);

    // Push one TypeCheck per op.
    let attestations: Vec<Attestation> = chain.iter()
        .map(|r| {
            let stage = match &r.op.kind {
                OperationKind::AddFunction { stage_id, .. } => stage_id.clone(),
                OperationKind::ModifyBody { to_stage_id, .. } => to_stage_id.clone(),
                _ => unreachable!(),
            };
            typecheck(&stage, &r.op_id)
        })
        .collect();
    let body = serde_json::to_string(&attestations).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/attestations/batch", &body);
    assert_eq!(status, 200);

    // Pull with no cutoff: get every attestation.
    let (status, body) = http(&srv.addr, "GET", "/v1/attestations/since", "");
    assert_eq!(status, 200, "{body}");
    let all: Vec<Attestation> = serde_json::from_str(&body).unwrap();
    assert_eq!(all.len(), 3);

    // Pull with after-op = chain[1]: get only attestations whose
    // op_id is *not* in chain[1]'s ancestry. That's just the
    // attestation on chain[2] (chain[0] and chain[1] are
    // ancestors of chain[1]).
    let cutoff = &chain[1].op_id;
    let path = format!("/v1/attestations/since?after-op={cutoff}");
    let (_, body) = http(&srv.addr, "GET", &path, "");
    let after: Vec<Attestation> = serde_json::from_str(&body).unwrap();
    assert_eq!(after.len(), 1, "only the attestation on chain[2] should remain");
    assert_eq!(after[0].op_id.as_deref(), Some(chain[2].op_id.as_str()));
}

#[test]
fn attestations_with_no_op_id_ship_regardless_of_cutoff() {
    let srv = start_server();
    let chain = seed_remote_chain(&srv, 2);

    // An Override attestation has op_id: None — doesn't participate
    // in the cutoff.
    let stage_id = match &chain[0].op.kind {
        OperationKind::AddFunction { stage_id, .. } => stage_id.clone(),
        _ => unreachable!(),
    };
    let override_att = Attestation::with_timestamp(
        stage_id,
        None, // no op_id
        None,
        AttestationKind::Override {
            actor: "alice".into(),
            reason: "manual".into(),
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
    let body = serde_json::to_string(&vec![override_att.clone()]).unwrap();
    let (status, _) = http(&srv.addr, "POST", "/v1/attestations/batch", &body);
    assert_eq!(status, 200);

    // Pull with after-op = remote head: cutoff would normally
    // include chain[0]'s ancestry. The Override's op_id is None,
    // so it always ships.
    let cutoff = &chain.last().unwrap().op_id;
    let path = format!("/v1/attestations/since?after-op={cutoff}");
    let (_, body) = http(&srv.addr, "GET", &path, "");
    let received: Vec<Attestation> = serde_json::from_str(&body).unwrap();
    let has_override = received.iter()
        .any(|a| matches!(a.kind, AttestationKind::Override { .. }));
    assert!(has_override, "Override attestation must ship regardless of after-op cutoff");
}
