//! #1007 PR 7 §1 over HTTP: merging disagreeing files manifests through the
//! same stateful `/v1/merge/{start,<id>/resolve,<id>/commit}` flow #977
//! already uses for sig/lock conflicts. A path edited on only one side
//! auto-resolves into a union manifest with no conflict; a path edited
//! *differently* on both sides surfaces as a `FileConflict` that blocks
//! commit until resolved; manifests that already agree need no `SetFiles`
//! at all.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use lex_api::handlers::State;
use lex_store::files::{Entry, Manifest};
use lex_store::{ManifestAt, Store, DEFAULT_BRANCH as MAIN};
use lex_vcs::{OpLog, Operation, OperationKind, OperationRecord, StageTransition};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    root: TempDir,
}

fn start_server() -> Server {
    let root = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => panic!("expected IP listener"),
    };
    let state = Arc::new(State::open(root.path().to_path_buf()).unwrap());
    thread::spawn(move || lex_api::serve_on(server, state));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok((200, _)) = try_http(&addr, "GET", "/v1/health", "") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "test server never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    Server { addr, root }
}

fn try_http(addr: &SocketAddr, method: &str, path: &str, body: &str) -> Result<(u16, String), String> {
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut s = TcpStream::connect_timeout(addr, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(15))).map_err(|e| e.to_string())?;
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).map_err(|e| e.to_string())?;
    if buf.is_empty() {
        return Err("empty response".into());
    }
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    Ok((status, body.to_string()))
}

fn http(srv: &Server, method: &str, path: &str, body: &str) -> (u16, Value) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match try_http(&srv.addr, method, path, body) {
            Ok((status, b)) => {
                let v = serde_json::from_str(&b).unwrap_or(Value::String(b));
                return (status, v);
            }
            Err(e) if std::time::Instant::now() >= deadline => {
                panic!("http {method} {path} failed after retries: {e}")
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn wire(bytes: &[u8]) -> Value {
    json!({ "id": sha(bytes), "data_b64": base64::engine::general_purpose::STANDARD.encode(bytes) })
}

fn upload(srv: &Server, blobs: &[Vec<u8>]) {
    let body: Vec<Value> = blobs.iter().map(|b| wire(b)).collect();
    let (st, v) = http(srv, "POST", "/v1/blobs/batch", &Value::Array(body).to_string());
    assert_eq!(st, 200, "{v}");
}

/// A manifest over `files` and the bytes of every blob it needs (the
/// manifest blob first), without uploading anything.
fn manifest(files: &[(&str, &[u8])]) -> (String, Vec<Vec<u8>>) {
    let mut m = Manifest::new();
    let mut blobs = Vec::new();
    for (path, bytes) in files {
        m.entries.insert(
            path.to_string(),
            Entry { blob: sha(bytes), mode: "100644".into(), size: bytes.len() as u64 },
        );
        blobs.push(bytes.to_vec());
    }
    let canon = m.to_canonical_bytes();
    blobs.insert(0, canon);
    (m.id(), blobs)
}

fn set_files_rec(manifest: &str, parents: Vec<String>) -> OperationRecord {
    OperationRecord::new(
        Operation::new(OperationKind::SetFiles { manifest: manifest.into() }, parents),
        StageTransition::FilesOnly,
    )
}

fn add_fn_rec(sig: &str, parents: Vec<String>) -> OperationRecord {
    OperationRecord::new(
        Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: format!("stage-{sig}"),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            parents,
        ),
        StageTransition::Create { sig_id: sig.into(), stage_id: format!("stage-{sig}") },
    )
}

fn push_ops(srv: &Server, recs: &[&OperationRecord]) -> (u16, Value) {
    http(srv, "POST", "/v1/ops/batch", &serde_json::to_string(recs).unwrap())
}

fn advance(srv: &Server, branch: &str, head: &str) -> (u16, Value) {
    http(srv, "POST", &format!("/v1/branches/{branch}/head"), &json!({ "head_op": head }).to_string())
}

fn branch_head(srv: &Server, branch: &str) -> Option<String> {
    Store::open(srv.root.path()).unwrap().get_branch(branch).unwrap().and_then(|b| b.head_op)
}

fn create_branch(srv: &Server, name: &str, from: &str) {
    Store::open(srv.root.path()).unwrap().create_branch(name, from).unwrap();
}

fn merge_start(srv: &Server, src: &str, dst: &str) -> Value {
    let (st, v) = http(srv, "POST", "/v1/merge/start",
        &json!({"src_branch": src, "dst_branch": dst}).to_string());
    assert_eq!(st, 200, "{v}");
    v
}

fn op_kind(srv: &Server, op_id: &str) -> String {
    let rec = OpLog::open(srv.root.path()).unwrap().get(&op_id.to_string()).unwrap()
        .unwrap_or_else(|| panic!("op {op_id} not found"));
    match rec.op.kind {
        OperationKind::SetFiles { .. } => "set_files".to_string(),
        OperationKind::Merge { .. } => "merge".to_string(),
        other => format!("{other:?}"),
    }
}

// ── Two branches that never touch files at all: pure sig-level merge, no
//    files dimension enters the picture (regression guard: attaching an
//    empty files diff must not change existing #977/#834 behavior). ──────

#[test]
fn no_files_at_all_is_unaffected() {
    // Regression guard for #977/#834: a merge session with no files
    // dimension at all (neither branch ever recorded a `SetFiles`) must
    // report empty file conflicts and `needs_setfiles: false` — attaching
    // an empty diff must not perturb the existing sig-only behavior.
    // `add_fn_rec` below builds bare `AddFunction` ops with no real
    // published stage content, which is enough for `merge/start`'s
    // (structural) diff but not for `merge/commit`'s type-check gate — so
    // this test, like `blobs_sync_1007.rs`'s ambiguous-manifest test,
    // stops at `merge/start` rather than trying to land the sig merge.
    let srv = start_server();
    let base = add_fn_rec("fn::base", vec![]);
    assert_eq!(push_ops(&srv, &[&base]).0, 200);
    assert_eq!(advance(&srv, MAIN, &base.op_id).0, 200);
    create_branch(&srv, "feature", MAIN);

    let a = add_fn_rec("fn::a", vec![base.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&a]).0, 200);
    assert_eq!(advance(&srv, MAIN, &a.op_id).0, 200);

    let b = add_fn_rec("fn::b", vec![base.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&b]).0, 200);
    assert_eq!(advance(&srv, "feature", &b.op_id).0, 200);

    let v = merge_start(&srv, "feature", MAIN);
    assert!(v["conflicts"].as_array().unwrap().is_empty());
    assert!(v["file_conflicts"].as_array().unwrap().is_empty());
    assert_eq!(v["needs_setfiles"], false);
}

fn publish(srv: &Server, src: &str) {
    let (st, v) = http(srv, "POST", "/v1/publish", &json!({"source": src, "activate": true}).to_string());
    assert_eq!(st, 200, "publish: {v}");
}

/// A merge that is real at the sig level (disjoint published functions,
/// so `apply_merge_op_gated`'s type-check gate has real stages to load)
/// but whose files manifests already agree — the #1007 §1 "files agree
/// ⇒ no SetFiles at all" case, exercised over the full HTTP commit path
/// rather than only at the `Store::manifest_merge` unit level.
#[test]
fn identical_manifests_produce_no_setfiles_even_with_a_real_sig_merge() {
    let srv = start_server();
    let (m0, b0) = manifest(&[("README.md", b"same everywhere")]);
    upload(&srv, &b0);

    publish(&srv, "fn foo(n :: Int) -> Int { n }\n");
    let main_after_publish = branch_head(&srv, MAIN).unwrap();
    let main_sf = set_files_rec(&m0, vec![main_after_publish]);
    assert_eq!(push_ops(&srv, &[&main_sf]).0, 200);
    assert_eq!(advance(&srv, MAIN, &main_sf.op_id).0, 200);

    let (st, v) = http(&srv, "POST", "/v1/branches",
        &json!({"name": "feature", "checkout": true}).to_string());
    assert_eq!(st, 201, "create feature: {v}");
    publish(&srv, "fn foo(n :: Int) -> Int { n }\nfn bar(n :: Int) -> Int { n + 1 }\n");
    let feature_after_publish = branch_head(&srv, "feature").unwrap();
    let feature_sf = set_files_rec(&m0, vec![feature_after_publish]);
    assert_eq!(push_ops(&srv, &[&feature_sf]).0, 200);
    assert_eq!(advance(&srv, "feature", &feature_sf.op_id).0, 200);
    assert_eq!(http(&srv, "POST", "/v1/branches/main/checkout", "").0, 200);

    let v = merge_start(&srv, "feature", MAIN);
    assert!(v["conflicts"].as_array().unwrap().is_empty(), "disjoint funcs: {v}");
    assert!(v["file_conflicts"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["needs_setfiles"], false, "identical manifests need no SetFiles: {v}");
    let merge_id = v["merge_id"].as_str().unwrap();

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(st, 200, "{v}");
    let new_head = v["new_head_op"].as_str().unwrap();
    assert_eq!(op_kind(&srv, new_head), "merge", "no spurious SetFiles when manifests already agree");

    let store = Store::open(srv.root.path()).unwrap();
    assert_eq!(store.manifest_at(new_head).unwrap(), ManifestAt::Set { manifest: m0 });
}

// ── Disjoint file adds: each side adds a DIFFERENT path. Auto-resolves;
//    no FileConflict is surfaced; the merge still needs a SetFiles
//    recording the union. ─────────────────────────────────────────────

#[test]
fn disjoint_file_adds_auto_resolve_and_merge_with_union_manifest() {
    let srv = start_server();
    let (m0, b0) = manifest(&[("A.txt", b"a")]);
    upload(&srv, &b0);
    let base = set_files_rec(&m0, vec![]);
    assert_eq!(push_ops(&srv, &[&base]).0, 200);
    assert_eq!(advance(&srv, MAIN, &base.op_id).0, 200);
    create_branch(&srv, "feature", MAIN);

    // dst (main) adds C.txt.
    let (m_ours, b_ours) = manifest(&[("A.txt", b"a"), ("C.txt", b"c")]);
    upload(&srv, &b_ours);
    let ours = set_files_rec(&m_ours, vec![base.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&ours]).0, 200);
    assert_eq!(advance(&srv, MAIN, &ours.op_id).0, 200);

    // src (feature) adds B.txt.
    let (m_theirs, b_theirs) = manifest(&[("A.txt", b"a"), ("B.txt", b"b")]);
    upload(&srv, &b_theirs);
    let theirs = set_files_rec(&m_theirs, vec![base.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&theirs]).0, 200);
    assert_eq!(advance(&srv, "feature", &theirs.op_id).0, 200);

    let v = merge_start(&srv, "feature", MAIN);
    assert!(v["conflicts"].as_array().unwrap().is_empty());
    assert!(v["file_conflicts"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["needs_setfiles"], true);
    let merge_id = v["merge_id"].as_str().unwrap();

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(st, 200, "{v}");
    let new_head = v["new_head_op"].as_str().unwrap();
    assert_eq!(op_kind(&srv, new_head), "set_files", "the merge must land a SetFiles on top");

    let store = Store::open(srv.root.path()).unwrap();
    match store.manifest_at(new_head).unwrap() {
        ManifestAt::Set { manifest } => {
            let m = store.get_manifest(&manifest).unwrap();
            assert_eq!(m.entries.len(), 3, "{m:?}");
            assert!(m.entries.contains_key("A.txt"));
            assert!(m.entries.contains_key("B.txt"));
            assert!(m.entries.contains_key("C.txt"));
        }
        other => panic!("expected a resolved manifest, got {other:?}"),
    }
    assert_eq!(branch_head(&srv, MAIN).as_deref(), Some(new_head));
}

// ── Same path, different edits: a real FileConflict. Blocks commit until
//    resolved; take_ours / take_theirs each produce the expected
//    manifest. ─────────────────────────────────────────────────────────

fn diverged_same_path(srv: &Server) -> (String, String, String) {
    let (m0, b0) = manifest(&[("README.md", b"base")]);
    upload(srv, &b0);
    let base = set_files_rec(&m0, vec![]);
    assert_eq!(push_ops(srv, &[&base]).0, 200);
    assert_eq!(advance(srv, MAIN, &base.op_id).0, 200);
    create_branch(srv, "feature", MAIN);

    let (m_ours, b_ours) = manifest(&[("README.md", b"left")]);
    upload(srv, &b_ours);
    let ours = set_files_rec(&m_ours, vec![base.op_id.clone()]);
    assert_eq!(push_ops(srv, &[&ours]).0, 200);
    assert_eq!(advance(srv, MAIN, &ours.op_id).0, 200);

    let (m_theirs, b_theirs) = manifest(&[("README.md", b"right")]);
    upload(srv, &b_theirs);
    let theirs = set_files_rec(&m_theirs, vec![base.op_id.clone()]);
    assert_eq!(push_ops(srv, &[&theirs]).0, 200);
    assert_eq!(advance(srv, "feature", &theirs.op_id).0, 200);

    let v = merge_start(srv, "feature", MAIN);
    let merge_id = v["merge_id"].as_str().unwrap().to_string();
    let file_conflicts = v["file_conflicts"].as_array().unwrap();
    assert_eq!(file_conflicts.len(), 1, "{v}");
    assert_eq!(file_conflicts[0]["path"], "README.md");
    assert_eq!(v["needs_setfiles"], true);
    (merge_id, ours.op_id, theirs.op_id)
}

#[test]
fn same_path_conflict_blocks_commit_until_resolved() {
    let srv = start_server();
    let (merge_id, _ours, _theirs) = diverged_same_path(&srv);

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "file conflicts remaining", "{v}");
    assert_eq!(v["detail"]["unresolved_files"].as_array().unwrap().len(), 1);

    // dst branch must be untouched.
    let store = Store::open(srv.root.path()).unwrap();
    assert!(matches!(store.manifest_at(&branch_head(&srv, MAIN).unwrap()).unwrap(), ManifestAt::Set { .. }));
}

#[test]
fn same_path_conflict_resolved_take_ours_lands_left() {
    let srv = start_server();
    let (merge_id, _ours, _theirs) = diverged_same_path(&srv);

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/resolve"),
        &json!({
            "resolutions": [],
            "file_resolutions": [{"path": "README.md", "resolution": {"kind": "take_ours"}}],
        }).to_string());
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["file_verdicts"][0]["accepted"], true, "{v}");
    assert!(v["remaining_file_conflicts"].as_array().unwrap().is_empty());

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(st, 200, "{v}");
    let new_head = v["new_head_op"].as_str().unwrap();

    let store = Store::open(srv.root.path()).unwrap();
    let m = match store.manifest_at(new_head).unwrap() {
        ManifestAt::Set { manifest } => store.get_manifest(&manifest).unwrap(),
        other => panic!("expected a resolved manifest, got {other:?}"),
    };
    let blob = &m.entries["README.md"].blob;
    assert_eq!(store.get_blob(blob).unwrap(), "left", "take_ours must keep dst's content");
}

#[test]
fn same_path_conflict_resolved_take_theirs_lands_right() {
    let srv = start_server();
    let (merge_id, _ours, _theirs) = diverged_same_path(&srv);

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/resolve"),
        &json!({
            "resolutions": [],
            "file_resolutions": [{"path": "README.md", "resolution": {"kind": "take_theirs"}}],
        }).to_string());
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["file_verdicts"][0]["accepted"], true, "{v}");

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(st, 200, "{v}");
    let new_head = v["new_head_op"].as_str().unwrap();

    let store = Store::open(srv.root.path()).unwrap();
    let m = match store.manifest_at(new_head).unwrap() {
        ManifestAt::Set { manifest } => store.get_manifest(&manifest).unwrap(),
        other => panic!("expected a resolved manifest, got {other:?}"),
    };
    let blob = &m.entries["README.md"].blob;
    assert_eq!(store.get_blob(blob).unwrap(), "right", "take_theirs must keep src's content");
}

#[test]
fn defer_on_a_file_conflict_still_blocks_commit() {
    let srv = start_server();
    let (merge_id, _ours, _theirs) = diverged_same_path(&srv);

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/resolve"),
        &json!({
            "resolutions": [],
            "file_resolutions": [{"path": "README.md", "resolution": {"kind": "defer"}}],
        }).to_string());
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["file_verdicts"][0]["accepted"], true, "defer is a valid, if unhelpful, resolution");
    assert_eq!(v["remaining_file_conflicts"].as_array().unwrap().len(), 1, "defer leaves it pending");

    let (st, v) = http(&srv, "POST", &format!("/v1/merge/{merge_id}/commit"), "");
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "file conflicts remaining", "{v}");
}
