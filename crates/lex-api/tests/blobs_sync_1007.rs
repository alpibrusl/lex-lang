//! #1007 PR 3: files beside the op-log, over HTTP — the blob routes
//! (`/v1/blobs/{missing,batch,fetch}`), the `/v1/ops/batch` closure check
//! for `SetFiles`, the head-advance gate, blob limits, and the `files-v1`
//! capability (`/v1/health` + the `/v1/ops/since` 426).

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use lex_api::handlers::{BlobLimits, State};
use lex_store::files::{Entry, Manifest};
use lex_vcs::{OpLog, Operation, OperationKind, OperationRecord, StageTransition};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Server {
    addr: SocketAddr,
    root: TempDir,
}

fn start(limits: Option<BlobLimits>) -> Server {
    let root = TempDir::new().unwrap();
    let server = tiny_http::Server::http(("127.0.0.1", 0)).expect("bind ephemeral port");
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(a) => a,
        _ => panic!("expected IP listener"),
    };
    let state = State::open(root.path().to_path_buf())
        .unwrap()
        .with_blob_limits(limits);
    let state = Arc::new(state);
    thread::spawn(move || lex_api::serve_on(server, state));
    thread::sleep(Duration::from_millis(20));
    Server { addr, root }
}

fn start_default() -> Server {
    start(None)
}

fn http_h(srv: &Server, method: &str, path: &str, body: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let (status, _, v) = http_full(srv, method, path, body, headers);
    (status, v)
}

/// As [`http_h`], plus the response headers (lowercased names).
fn http_full(srv: &Server, method: &str, path: &str, body: &str, headers: &[(&str, &str)])
    -> (u16, Vec<(String, String)>, Value)
{
    let mut s = TcpStream::connect(srv.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap_or((&buf, ""));
    let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    let resp_headers = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, resp_headers, serde_json::from_str(body).unwrap_or(Value::String(body.to_string())))
}

fn http(srv: &Server, method: &str, path: &str, body: &str) -> (u16, Value) {
    http_h(srv, method, path, body, &[])
}

fn sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

fn wire(bytes: &[u8]) -> Value {
    json!({ "id": sha(bytes), "data_b64": base64::engine::general_purpose::STANDARD.encode(bytes) })
}

fn missing(srv: &Server, ids: &[&str]) -> Vec<String> {
    let (st, v) = http(srv, "POST", "/v1/blobs/missing", &json!({ "ids": ids }).to_string());
    assert_eq!(st, 200, "{v}");
    serde_json::from_value(v["missing"].clone()).unwrap()
}

fn batch(srv: &Server, blobs: &[&[u8]]) -> (u16, Value) {
    let body: Vec<Value> = blobs.iter().map(|b| wire(b)).collect();
    http(srv, "POST", "/v1/blobs/batch", &Value::Array(body).to_string())
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

fn upload(srv: &Server, blobs: &[Vec<u8>]) {
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let (st, v) = batch(srv, &refs);
    assert_eq!(st, 200, "{v}");
}

fn add_fn(sig: &str, parents: Vec<String>) -> OperationRecord {
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

fn set_files(manifest: &str, parents: Vec<String>) -> OperationRecord {
    OperationRecord::new(
        Operation::new(OperationKind::SetFiles { manifest: manifest.into() }, parents),
        StageTransition::FilesOnly,
    )
}

fn merge(parents: Vec<String>) -> OperationRecord {
    OperationRecord::new(
        Operation::new(OperationKind::Merge { resolved: 0 }, parents),
        StageTransition::Merge { entries: Default::default() },
    )
}

fn push_ops(srv: &Server, recs: &[&OperationRecord]) -> (u16, Value) {
    http(srv, "POST", "/v1/ops/batch", &serde_json::to_string(recs).unwrap())
}

fn advance(srv: &Server, head: &str) -> (u16, Value) {
    http(srv, "POST", "/v1/branches/main/head", &json!({ "head_op": head }).to_string())
}

fn remote_head(srv: &Server) -> Value {
    http(srv, "GET", "/v1/branches/main/head", "").1["head_op"].clone()
}

fn op_known(srv: &Server, op_id: &str) -> bool {
    OpLog::open(srv.root.path()).unwrap().get(&op_id.to_string()).unwrap().is_some()
}

// ── capability advertisement ───────────────────────────────────────────────

#[test]
fn health_advertises_files_v1() {
    let srv = start_default();
    let (st, v) = http(&srv, "GET", "/v1/health", "");
    assert_eq!(st, 200);
    assert_eq!(v["ok"], true);
    assert_eq!(v["caps"], json!(["files-v1", "intent-origin-v1"]));
}

// ── blob routes ────────────────────────────────────────────────────────────

#[test]
fn blob_batch_then_fetch_round_trips_exact_bytes() {
    let srv = start_default();
    let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff, 0xfe];
    let readme: &[u8] = b"# hello\n";
    let (png_id, readme_id) = (sha(png), sha(readme));

    assert_eq!(missing(&srv, &[&png_id, &readme_id]), vec![png_id.clone(), readme_id.clone()]);

    let (st, v) = batch(&srv, &[png, readme]);
    assert_eq!(st, 200, "{v}");
    assert_eq!((v["received"].as_u64(), v["added"].as_u64()), (Some(2), Some(2)));
    assert!(missing(&srv, &[&png_id, &readme_id]).is_empty());

    // Idempotent: a re-push adds nothing.
    let (st, v) = batch(&srv, &[png, png]);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["added"], 0);

    let absent = sha(b"never uploaded");
    let (st, v) = http(
        &srv,
        "POST",
        "/v1/blobs/fetch",
        &json!({ "ids": [png_id, absent, readme_id] }).to_string(),
    );
    assert_eq!(st, 200, "{v}");
    let got: Vec<(String, Vec<u8>)> = v["blobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b["data_b64"].as_str().unwrap())
                .unwrap();
            (b["id"].as_str().unwrap().to_string(), bytes)
        })
        .collect();
    assert_eq!(got, vec![(png_id, png.to_vec()), (readme_id, readme.to_vec())], "absent ids are omitted");
}

#[test]
fn blob_batch_with_one_mismatched_id_writes_nothing() {
    let srv = start_default();
    let good: &[u8] = b"good bytes";
    let bad: &[u8] = b"bad bytes";
    let forged = sha(b"something else");
    let body = json!([
        wire(good),
        { "id": forged, "data_b64": base64::engine::general_purpose::STANDARD.encode(bad) },
    ]);
    let (st, v) = http(&srv, "POST", "/v1/blobs/batch", &body.to_string());
    assert_eq!(st, 409, "{v}");
    assert_eq!(v["error"], "BlobIdMismatch");
    assert_eq!(v["detail"]["mismatches"][0]["id"], forged);
    assert_eq!(v["detail"]["mismatches"][0]["actual"], sha(bad));
    // The good entry — listed *before* the bad one — was not written either.
    assert_eq!(missing(&srv, &[&sha(good), &forged]), vec![sha(good), forged]);
}

#[test]
fn oversize_blob_is_413_and_writes_nothing() {
    let limits = BlobLimits { max_blob_bytes: 8, store_quota_bytes: 1 << 20, max_manifest_entries: 100 };
    let srv = start(Some(limits));
    let ok: &[u8] = b"12345678"; // exactly the limit
    let big: &[u8] = b"123456789";
    let (st, v) = batch(&srv, &[ok, big]);
    assert_eq!(st, 413, "{v}");
    assert_eq!(v["error"], "BlobTooLarge");
    assert_eq!(v["detail"]["id"], sha(big));
    assert_eq!(missing(&srv, &[&sha(ok)]), vec![sha(ok)], "all-or-nothing");
    // Negative control: at the limit is accepted.
    let (st, v) = batch(&srv, &[ok]);
    assert_eq!(st, 200, "{v}");
}

#[test]
fn over_quota_is_507_and_already_held_bytes_do_not_count() {
    let limits = BlobLimits { max_blob_bytes: 1 << 20, store_quota_bytes: 10, max_manifest_entries: 100 };
    let srv = start(Some(limits));
    let a: &[u8] = b"aaaaaa"; // 6
    let b: &[u8] = b"bbbbbb"; // 6 → 12 > 10
    let (st, v) = batch(&srv, &[a]);
    assert_eq!(st, 200, "{v}");
    let (st, v) = batch(&srv, &[b]);
    assert_eq!(st, 507, "{v}");
    assert_eq!(v["error"], "BlobQuotaExceeded");
    assert_eq!((v["detail"]["used"].as_u64(), v["detail"]["incoming"].as_u64()), (Some(6), Some(6)));
    assert_eq!(missing(&srv, &[&sha(b)]), vec![sha(b)]);
    // Re-sending what the store already holds costs nothing.
    let (st, v) = batch(&srv, &[a, a]);
    assert_eq!(st, 200, "{v}");
    // A batch that only fits with the duplicate counted once is fine too.
    let c: &[u8] = b"cccc"; // 6 + 4 == 10
    let (st, v) = batch(&srv, &[c, c]);
    assert_eq!(st, 200, "{v}");
}

// ── /v1/ops/batch closure check ────────────────────────────────────────────

#[test]
fn set_files_op_without_its_blobs_is_422_and_not_persisted() {
    let srv = start_default();
    let (mid, blobs) = manifest(&[("README.md", b"# hi\n"), ("tests/a.lex", b"t")]);
    let base = add_fn("f", vec![]);
    let sf = set_files(&mid, vec![base.op_id.clone()]);

    // Nothing uploaded: the manifest itself is missing.
    let (st, v) = push_ops(&srv, &[&base, &sf]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "MissingBlobs");
    assert_eq!(v["detail"]["op_id"], sf.op_id);
    assert_eq!(v["detail"]["ids"], json!([mid]));
    assert!(!op_known(&srv, &base.op_id), "whole batch refused, nothing persisted");

    // Manifest present, one entry missing: exactly that entry is named.
    upload(&srv, &blobs[..2]);
    let (st, v) = push_ops(&srv, &[&base, &sf]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["detail"]["ids"], json!([sha(&blobs[2])]));
    assert!(!op_known(&srv, &sf.op_id));

    // Complete closure: accepted.
    upload(&srv, &blobs[2..]);
    let (st, v) = push_ops(&srv, &[&base, &sf]);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["added"], 2);
    assert!(op_known(&srv, &sf.op_id));
}

#[test]
fn set_files_op_with_a_bad_manifest_is_422_invalid_manifest() {
    let srv = start_default();
    // Present, but not a canonical manifest (pretty-printed JSON).
    let readme: &[u8] = b"x";
    let mut m = Manifest::new();
    m.entries.insert("README.md".into(), Entry { blob: sha(readme), mode: "100644".into(), size: 1 });
    let pretty = serde_json::to_vec_pretty(&m).unwrap();
    upload(&srv, &[pretty.clone(), readme.to_vec()]);
    let sf = set_files(&sha(&pretty), vec![]);
    let (st, v) = push_ops(&srv, &[&sf]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "InvalidManifest");
    assert_eq!(v["detail"]["op_id"], sf.op_id);

    // A manifest claiming a src/*.lex path (op-log owned).
    let (mid, blobs) = manifest(&[("src/main.lex", b"fn f() -> Int { 1 }")]);
    upload(&srv, &blobs);
    let (st, v) = push_ops(&srv, &[&set_files(&mid, vec![])]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "InvalidManifest");

    // Size in the manifest disagrees with the blob.
    let mut lying = Manifest::new();
    lying.entries.insert("README.md".into(), Entry { blob: sha(readme), mode: "100644".into(), size: 99 });
    upload(&srv, &[lying.to_canonical_bytes()]);
    let (st, v) = push_ops(&srv, &[&set_files(&lying.id(), vec![])]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "InvalidManifest");

    // A SetFiles claiming a stage transition could rewrite the head map.
    let (mid, blobs) = manifest(&[("README.md", b"ok")]);
    upload(&srv, &blobs);
    let mut forged = set_files(&mid, vec![]);
    forged.produces = StageTransition::Create { sig_id: "f".into(), stage_id: "s".into() };
    let (st, v) = push_ops(&srv, &[&forged]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "InvalidTransition");

    // Negative control: the honest op over the same manifest is accepted.
    let (st, v) = push_ops(&srv, &[&set_files(&mid, vec![])]);
    assert_eq!(st, 200, "{v}");
}

#[test]
fn manifest_entry_limit_is_enforced_on_ops_batch() {
    let limits = BlobLimits { max_blob_bytes: 1 << 20, store_quota_bytes: 1 << 20, max_manifest_entries: 2 };
    let srv = start(Some(limits));
    let (three, blobs) = manifest(&[("a", b"1"), ("b", b"2"), ("c", b"3")]);
    upload(&srv, &blobs);
    let (st, v) = push_ops(&srv, &[&set_files(&three, vec![])]);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "InvalidManifest");
    let (two, blobs) = manifest(&[("a", b"1"), ("b", b"2")]);
    upload(&srv, &blobs);
    let (st, v) = push_ops(&srv, &[&set_files(&two, vec![])]);
    assert_eq!(st, 200, "{v}");
}

// ── /v1/ops/since compatibility ────────────────────────────────────────────

fn since(srv: &Server, caps: Option<&str>) -> (u16, Value) {
    let headers: Vec<(&str, &str)> = caps.map(|c| ("X-Lex-Caps", c)).into_iter().collect();
    http_h(srv, "GET", "/v1/ops/since?branch=main", "", &headers)
}

#[test]
fn ops_since_refuses_set_files_to_a_client_without_files_v1() {
    let srv = start_default();
    let base = add_fn("f", vec![]);
    let (st, v) = push_ops(&srv, &[&base]);
    assert_eq!(st, 200, "{v}");
    assert_eq!(advance(&srv, &base.op_id).0, 200);

    // No SetFiles in history: 200 regardless of caps.
    for caps in [None, Some("files-v1")] {
        let (st, v) = since(&srv, caps);
        assert_eq!(st, 200, "caps {caps:?}: {v}");
        assert_eq!(v.as_array().unwrap().len(), 1);
    }

    let (mid, blobs) = manifest(&[("README.md", b"# r\n")]);
    upload(&srv, &blobs);
    let sf = set_files(&mid, vec![base.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&sf]).0, 200);
    assert_eq!(advance(&srv, &sf.op_id).0, 200);

    let (st, v) = since(&srv, None);
    assert_eq!(st, 426, "{v}");
    assert!(v["error"].as_str().unwrap().contains("upgrade lex"), "readable: {v}");
    assert_eq!(v["detail"]["required_cap"], "files-v1");
    assert_eq!(v["detail"]["op_id"], sf.op_id);
    // An unrelated cap doesn't count.
    assert_eq!(since(&srv, Some("something-else")).0, 426);

    for caps in ["files-v1", "other, files-v1"] {
        let (st, v) = since(&srv, Some(caps));
        assert_eq!(st, 200, "caps {caps}: {v}");
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    // A delta that doesn't reach the SetFiles is still fine for old clients:
    // `after` = the SetFiles itself → nothing new.
    let (st, _) = http(&srv, "GET", &format!("/v1/ops/since?branch=main&after={}", sf.op_id), "");
    assert_eq!(st, 200);
    // …but a `limit` that would stop short of it doesn't let an old client
    // start a pull it can't finish.
    let (st, _) = http(&srv, "GET", "/v1/ops/since?branch=main&limit=1", "");
    assert_eq!(st, 426);
}

// ── head advance ───────────────────────────────────────────────────────────

#[test]
fn head_advance_refuses_an_ambiguous_manifest() {
    let srv = start_default();
    let (m1, b1) = manifest(&[("README.md", b"left")]);
    let (m2, b2) = manifest(&[("README.md", b"right")]);
    upload(&srv, &b1);
    upload(&srv, &b2);
    let base = add_fn("f", vec![]);
    let left = set_files(&m1, vec![base.op_id.clone()]);
    let right = set_files(&m2, vec![base.op_id.clone()]);
    let merged = merge(vec![left.op_id.clone(), right.op_id.clone()]);
    let (st, v) = push_ops(&srv, &[&base, &left, &right, &merged]);
    assert_eq!(st, 200, "{v}");

    // Negative control: a single-manifest head advances.
    assert_eq!(advance(&srv, &left.op_id).0, 200);

    let (st, v) = advance(&srv, &merged.op_id);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "AmbiguousManifest");
    assert_eq!(v["detail"]["head_op"], merged.op_id);
    assert_eq!(remote_head(&srv), json!(left.op_id), "head unchanged");

    // Resolved by a SetFiles recording the merged manifest.
    let (m3, b3) = manifest(&[("README.md", b"left+right")]);
    upload(&srv, &b3);
    let resolved = set_files(&m3, vec![merged.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&resolved]).0, 200);
    let (st, v) = advance(&srv, &resolved.op_id);
    assert_eq!(st, 200, "{v}");
    assert_eq!(remote_head(&srv), json!(resolved.op_id));
}

#[test]
fn head_advance_refuses_a_manifest_whose_blobs_are_absent() {
    let srv = start_default();
    // Land a SetFiles in the log behind `ops/batch`'s back (a store written
    // by an older server, or blobs lost since): the head gate still holds.
    let (mid, blobs) = manifest(&[("README.md", b"gone")]);
    let sf = set_files(&mid, vec![]);
    OpLog::open(srv.root.path()).unwrap().put(&sf).unwrap();
    let (st, v) = advance(&srv, &sf.op_id);
    assert_eq!(st, 422, "{v}");
    assert_eq!(v["error"], "MissingBlobs");
    assert_eq!(remote_head(&srv), Value::Null);
    upload(&srv, &blobs);
    assert_eq!(advance(&srv, &sf.op_id).0, 200);
}

/// One page of `/v1/ops/since`: status, the `X-Lex-Next-Cursor` header (if
/// any), and the ops returned.
fn page(srv: &Server, query: &str, caps: Option<&str>) -> (u16, Option<String>, Value) {
    let headers: Vec<(&str, &str)> = caps.map(|c| ("X-Lex-Caps", c)).into_iter().collect();
    let (st, hs, v) = http_full(srv, "GET", &format!("/v1/ops/since?{query}"), "", &headers);
    let cursor = hs.iter().find(|(k, _)| k == "x-lex-next-cursor").map(|(_, v)| v.clone());
    (st, cursor, v)
}

#[test]
fn paged_ops_since_refuses_files_before_the_page_that_holds_them() {
    let srv = start_default();
    let a = add_fn("f", vec![]);
    let b = add_fn("g", vec![a.op_id.clone()]);
    let (mid, blobs) = manifest(&[("README.md", b"# r\n")]);
    upload(&srv, &blobs);
    let sf = set_files(&mid, vec![b.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&a, &b, &sf]).0, 200);
    assert_eq!(advance(&srv, &sf.op_id).0, 200);

    // A files-v1 client pages through all three ops, one per page.
    let (st, cursor1, v) = page(&srv, "branch=main&limit=1", Some("files-v1"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v.as_array().unwrap().len(), 1);
    let cursor1 = cursor1.expect("a page that leaves ops behind carries a cursor");
    let (st, cursor2, v) = page(&srv, &format!("cursor={cursor1}&limit=1"), Some("files-v1"));
    assert_eq!(st, 200, "{v}");
    let cursor2 = cursor2.expect("second page carries a cursor");
    let (st, _, v) = page(&srv, &format!("cursor={cursor2}&limit=1"), Some("files-v1"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v[0]["op_id"], sf.op_id, "the last page is the SetFiles");

    // The same first page without caps is refused, although the page itself
    // holds no SetFiles: the rest of the delta does.
    let (st, cursor, v) = page(&srv, "branch=main&limit=1", None);
    assert_eq!(st, 426, "{v}");
    assert_eq!(v["detail"]["op_id"], sf.op_id);
    assert_eq!(cursor, None, "a refusal is not a page");

    // And a resumed page that itself holds the SetFiles is refused too.
    let (st, _, v) = page(&srv, &format!("cursor={cursor2}&after={}&limit=1", b.op_id), None);
    assert_eq!(st, 426, "{v}");

    // Negative control: a resumed page with no SetFiles in it is served to
    // a client without caps (the look-ahead is a start-of-pull check).
    let (st, _, v) = page(&srv, &format!("cursor={cursor1}&limit=1"), None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v[0]["op_id"], b.op_id);
}

#[test]
fn paged_history_without_files_is_unaffected() {
    let srv = start_default();
    let a = add_fn("f", vec![]);
    let b = add_fn("g", vec![a.op_id.clone()]);
    assert_eq!(push_ops(&srv, &[&a, &b]).0, 200);
    assert_eq!(advance(&srv, &b.op_id).0, 200);
    let (st, cursor, v) = page(&srv, "branch=main&limit=1", None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v[0]["op_id"], a.op_id);
    let (st, _, v) = page(&srv, &format!("cursor={}&limit=1", cursor.unwrap()), None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v[0]["op_id"], b.op_id);
}
