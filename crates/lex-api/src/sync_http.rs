//! Object sync over HTTP — the content half of `op push`/`pull`.
//!
//! `/v1/ops/batch` transfers op *records* and `/v1/branches/<b>/head`
//! advances the ref, but an op record only *references* the stage (the
//! canonical AST) and intent it produced, by content-addressed id. Without
//! the referenced objects a pulled op-log is a history skeleton with no
//! code — `export-git`/replay fail with `unknown stage_id`. These endpoints
//! transfer the stage and intent blobs so a pushed package fully
//! round-trips.
//!
//! `batch` receives objects (content-addressed, so storage is idempotent
//! and self-verifying); `fetch` returns the objects for a set of ids the
//! puller already knows it needs (from the op records it just pulled).

use std::io::Cursor;
use tiny_http::Response;

use crate::handlers::{error_response, error_with_detail, json_response, BlobLimits, State};
use lex_ast::{stage_id, Stage};
use lex_store::{ManifestAt, Store, StoreError};
use lex_vcs::{Intent, IntentLog, Issue, IssueLog, OperationKind, OperationRecord, StageTransition};

/// `POST /v1/stages/batch` — receive stage blobs. Body: a JSON array of
/// `Stage`. Each is stored content-addressed (idempotent); a stage that
/// isn't hashable (an import) is skipped rather than erroring the batch.
pub(crate) fn stages_batch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let stages: Vec<Stage> = match serde_json::from_str(body) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("body must be a JSON array of Stage: {e}")),
    };
    let store = state.store.lock().unwrap();
    // One bulk existence probe for the whole batch (#971): `get_ast` per
    // stage re-reads and re-parses the entire stage index on every call.
    let ids: Vec<Option<String>> = stages.iter().map(stage_id).collect();
    let known: Vec<String> = ids.iter().flatten().cloned().collect();
    let mut present: std::collections::BTreeSet<String> = known
        .iter()
        .zip(store.get_asts_bulk(&known))
        .filter(|(_, got)| got.is_ok())
        .map(|(id, _)| id.clone())
        .collect();
    let (mut added, mut skipped) = (0usize, 0usize);
    for (stage, id) in stages.iter().zip(ids) {
        let id = match id {
            Some(id) => id,
            None => { skipped += 1; continue } // import / unhashable
        };
        // `insert` returns false when already present — including a
        // duplicate earlier in this same batch.
        let existed = !present.insert(id.clone());
        if let Err(e) = store.publish(stage) {
            return error_response(500, format!("publish stage {id}: {e}"));
        }
        if existed { skipped += 1 } else { added += 1 }
    }
    json_response(200, &serde_json::json!({
        "received": stages.len(), "added": added, "skipped": skipped,
    }))
}

/// `POST /v1/stages/fetch` — return stage blobs. Body: `{ "ids":
/// ["<stage_id>", ...] }`, or (#1060) `{ "pairs": [["<sig_id>", "<stage_id>"],
/// ...] }`. Returns a JSON array of the `Stage`s present (missing entries are
/// silently omitted; the caller reconciles).
///
/// A StageId does not encode the name (#826), so a rename leaves the *same*
/// StageId under two sigs holding two different ASTs. By id the store can only
/// answer with the one variant `stage_index` names, so a peer asking for the
/// renamed variant got the old one and filed it under the old sig — the pulled
/// head then named a `(sig, stage)` pair nothing had ever supplied. `pairs`
/// resolves each entry through the sig it names, exactly as the render does,
/// and wins when both keys are present (an up-to-date client sends both so an
/// older hub, which only reads `ids`, still answers).
pub(crate) fn stages_fetch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(arr) = v.get("pairs").and_then(|p| p.as_array()) {
            let pairs: Vec<(String, String)> = arr
                .iter()
                .filter_map(|p| {
                    let a = p.as_array()?;
                    Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_str()?.to_string()))
                })
                .collect();
            let store = state.store.lock().unwrap();
            let stages: Vec<Stage> = store
                .get_asts_for_sigs_bulk(&pairs)
                .into_iter()
                .filter_map(Result::ok)
                .collect();
            return json_response(200, &serde_json::json!({ "stages": stages }));
        }
    }
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    // Bulk, not `get_ast` per id (#971): the per-id path re-reads and
    // re-parses the whole `stage_index.jsonl` on every call, so a 256-id
    // fetch cost 256 full index parses — measured at ~2.2s against a
    // 30k-stage store (vs ~40ms bulk), and >25s on the hosted hub.
    let stages: Vec<Stage> = store.get_asts_bulk(&ids).into_iter().filter_map(Result::ok).collect();
    json_response(200, &serde_json::json!({ "stages": stages }))
}

/// `POST /v1/stages/missing` — existence check. Body: `{ "ids": ["<stage_id>",
/// …] }`. Returns `{ "missing": [<the ids this store does NOT have>] }`. A cheap
/// companion to `batch`/`fetch`: `op push` uses it to reconcile the full
/// stage-closure of the head it's advancing to — push only the blobs the
/// remote actually lacks — without downloading every stage body just to learn
/// which are present (which `fetch` would force). Idempotent, read-only.
pub(crate) fn stages_missing_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return error_response(400, format!("body must be JSON: {e}")),
    };
    let store = state.store.lock().unwrap();

    // Preferred shape (#986): `{ "pairs": [[sig_id, stage_id], …] }`.
    //
    // A StageId does not encode the name (#826), so two sigs can share one
    // stage id while holding *different* ASTs. Rendering resolves through the
    // `(sig, stage)` pair, so an id-only check can answer "present" while the
    // variant the head names is absent — which is exactly how #968's closure
    // reconciliation was defeated. Answer per pair.
    if let Some(arr) = v.get("pairs").and_then(|p| p.as_array()) {
        let pairs: Vec<(String, String)> = arr
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((a.first()?.as_str()?.to_string(), a.get(1)?.as_str()?.to_string()))
            })
            .collect();
        let have = store.get_asts_for_sigs_bulk(&pairs);
        let missing: Vec<serde_json::Value> = pairs
            .iter()
            .zip(have)
            .filter(|(_, got)| got.is_err())
            .map(|((sig, stage), _)| serde_json::json!([sig, stage]))
            .collect();
        return json_response(200, &serde_json::json!({ "missing": missing }));
    }

    // Legacy id-only shape, kept so an older `op push` still gets an answer.
    // Necessarily approximate: "present under *some* sig" is the most this
    // form can mean.
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let have = store.get_asts_bulk(&ids);
    let missing: Vec<String> = ids
        .into_iter()
        .zip(have)
        .filter(|(_, got)| got.is_err())
        .map(|(id, _)| id)
        .collect();
    json_response(200, &serde_json::json!({ "missing": missing }))
}

/// `POST /v1/intents/batch` — receive intent records (content-addressed).
pub(crate) fn intents_batch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let intents: Vec<Intent> = match serde_json::from_str(body) {
        Ok(i) => i,
        Err(e) => return error_response(400, format!("body must be a JSON array of Intent: {e}")),
    };
    let store = state.store.lock().unwrap();
    let log = match IntentLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening intent log: {e}")),
    };
    let mut added = 0usize;
    for intent in &intents {
        let existed = matches!(log.get(&intent.intent_id), Ok(Some(_)));
        if let Err(e) = log.put(intent) {
            return error_response(500, format!("put intent {}: {e}", intent.intent_id));
        }
        if !existed { added += 1 }
    }
    json_response(200, &serde_json::json!({
        "received": intents.len(), "added": added,
    }))
}

/// `POST /v1/intents/fetch` — return intent records for a set of ids.
pub(crate) fn intents_fetch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    let log = match IntentLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening intent log: {e}")),
    };
    let intents: Vec<Intent> = ids.iter().filter_map(|id| log.get(id).ok().flatten()).collect();
    json_response(200, &serde_json::json!({ "intents": intents }))
}

/// `POST /v1/locks/batch` — receive committed lockfiles. Body: a JSON array
/// of `{ "head_op": "<op>", "lock": "<toml>" }`. Each binds the lock to its
/// package head (content-addressed + idempotent, via `set_committed_lock`).
/// The committed lock pins the exact dependency versions and op-log heads a
/// head was built against (#930), so the write-time gate can resolve this
/// head's dependencies instead of requiring them inlined.
pub(crate) fn locks_batch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return error_response(400, format!("body must be JSON: {e}")),
    };
    let entries = match v.as_array() {
        Some(a) => a,
        None => return error_response(400, "body must be a JSON array of {head_op, lock}"),
    };
    let store = state.store.lock().unwrap();
    let mut added = 0usize;
    for e in entries {
        let head_op = match e.get("head_op").and_then(|x| x.as_str()) {
            Some(h) => h,
            None => return error_response(400, "each entry needs a string `head_op`"),
        };
        let lock = match e.get("lock").and_then(|x| x.as_str()) {
            Some(l) => l,
            None => return error_response(400, "each entry needs a string `lock`"),
        };
        if let Err(err) = store.set_committed_lock(head_op, lock) {
            return error_response(500, format!("store lock for {head_op}: {err}"));
        }
        added += 1;
    }
    json_response(200, &serde_json::json!({ "received": entries.len(), "added": added }))
}

/// `POST /v1/locks/fetch` — return committed lockfiles for a set of head ops.
/// Body: `{ "head_ops": [...] }`. Returns `{ "locks": { head_op: toml } }`
/// for those present; a head with no committed lock is silently omitted.
pub(crate) fn locks_fetch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return error_response(400, format!("body must be JSON: {e}")),
    };
    let ids = match v.get("head_ops").and_then(|i| i.as_array()) {
        Some(a) => a,
        None => return error_response(400, "missing array field `head_ops`"),
    };
    let store = state.store.lock().unwrap();
    let mut locks = serde_json::Map::new();
    for id in ids.iter().filter_map(|x| x.as_str()) {
        match store.committed_lock(id) {
            Ok(Some(toml)) => {
                locks.insert(id.to_string(), serde_json::Value::String(toml));
            }
            Ok(None) => {}
            Err(e) => return error_response(500, format!("read lock {id}: {e}")),
        }
    }
    json_response(200, &serde_json::json!({ "locks": locks }))
}

/// `POST /v1/issues/batch` — receive typed issues (#949, content-addressed:
/// idempotent, self-verifying). Issues travel with the package like stages,
/// intents and locks, so a peer that pulls the op-log also gets the work
/// items its intents reference — and the open ones nothing references yet.
pub(crate) fn issues_batch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let issues: Vec<Issue> = match serde_json::from_str(body) {
        Ok(i) => i,
        Err(e) => return error_response(400, format!("body must be a JSON array of Issue: {e}")),
    };
    let store = state.store.lock().unwrap();
    let log = match IssueLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening issue log: {e}")),
    };
    // Content-addressed: the id must be the hash of the content. Refuse the
    // whole batch before writing anything — the log is keyed by id and
    // idempotent on re-puts, so a record filed under an id its content
    // doesn't own would silently shadow (or squat on) the real one.
    if let Some(bad) = issues.iter().find(|i| !i.id_is_consistent()) {
        return error_response(
            400,
            format!(
                "issue {}: issue_id does not match its content (expected {})",
                bad.issue_id,
                bad.computed_id()
            ),
        );
    }
    let mut added = 0usize;
    for issue in &issues {
        let existed = matches!(log.get(&issue.issue_id), Ok(Some(_)));
        if let Err(e) = log.put(issue) {
            return error_response(500, format!("put issue {}: {e}", issue.issue_id));
        }
        if !existed { added += 1 }
    }
    json_response(200, &serde_json::json!({
        "received": issues.len(), "added": added,
    }))
}

/// `POST /v1/issues/fetch` — return issues for a set of ids. Body:
/// `{ "ids": [...] }`; missing ids are silently omitted.
pub(crate) fn issues_fetch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    let log = match IssueLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening issue log: {e}")),
    };
    let issues: Vec<Issue> = ids.iter().filter_map(|id| log.get(id).ok().flatten()).collect();
    json_response(200, &serde_json::json!({ "issues": issues }))
}

/// `GET /v1/issues/list` — every issue id in the log. An issue can exist
/// before any op references it (open work), so a puller lists the whole log
/// rather than only the ids reachable from pulled ops.
pub(crate) fn issues_list_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let log = match IssueLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening issue log: {e}")),
    };
    match log.list_ids() {
        Ok(ids) => json_response(200, &serde_json::json!({ "ids": ids })),
        Err(e) => error_response(500, format!("listing issues: {e}")),
    }
}

// ── #1007: files beside the op-log ──────────────────────────────────────────
//
// A `SetFiles { manifest }` op names a canonical manifest blob, which names
// one blob per file. Push order is blobs → ops → head, so `/v1/ops/batch`
// can refuse a `SetFiles` whose closure is not already here (and a head can
// never name files the store can't serve). Blobs are store-scoped: under
// lex-hub each tenant store has its own blob space, so `missing` can't reveal
// what another tenant holds.

/// One blob on the wire: its content address and its exact bytes, base64.
#[derive(serde::Serialize, serde::Deserialize)]
struct WireBlob {
    id: String,
    data_b64: String,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// `POST /v1/blobs/missing` — `{ "ids": [...] }` → `{ "missing": [...] }`,
/// the ids *this store* does not hold (anything that isn't a blob id counts
/// as missing). Read-only; lets `op push` upload only what the remote lacks.
pub(crate) fn blobs_missing_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let missing: Vec<String> = ids
        .into_iter()
        .filter(|id| seen.insert(id.clone()) && !store.has_blob(id))
        .collect();
    json_response(200, &serde_json::json!({ "missing": missing }))
}

/// `POST /v1/blobs/batch` — receive blobs. Body: `[{ "id", "data_b64" }]`.
///
/// All-or-nothing: every entry is decoded, size-checked and re-hashed
/// before any is written, so a bad entry leaves the store untouched.
///
/// * `400` — not that shape, or `data_b64` isn't base64.
/// * `413` `BlobTooLarge` — a blob exceeds `blob_limits.max_blob_bytes`.
/// * `409` `BlobIdMismatch` `{ mismatches: [{ id, actual }] }` — an entry's
///   bytes don't hash to its `id` (content addressing must hold over the
///   wire; a blob filed under a hash it doesn't own would shadow the real
///   one).
/// * `507` `BlobQuotaExceeded` — the new bytes would take the store past
///   `blob_limits.store_quota_bytes`. Bytes already present don't count.
///
/// Idempotent: a blob already held is skipped.
pub(crate) fn blobs_batch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    use base64::Engine as _;
    let wire: Vec<WireBlob> = match serde_json::from_str(body) {
        Ok(w) => w,
        Err(e) => return error_response(400, format!("body must be a JSON array of {{id, data_b64}}: {e}")),
    };
    let limits = state.blob_limits;
    let mut decoded: Vec<(String, Vec<u8>)> = Vec::with_capacity(wire.len());
    let mut mismatches = Vec::new();
    for w in wire {
        let bytes = match b64().decode(w.data_b64.as_bytes()) {
            Ok(b) => b,
            Err(e) => return error_response(400, format!("blob {}: data_b64 is not base64: {e}", w.id)),
        };
        if let Some(l) = limits {
            if bytes.len() as u64 > l.max_blob_bytes {
                return error_with_detail(413, "BlobTooLarge", serde_json::json!({
                    "id": w.id,
                    "size": bytes.len(),
                    "max_blob_bytes": l.max_blob_bytes,
                }));
            }
        }
        let actual = sha256_hex(&bytes);
        if actual != w.id {
            mismatches.push(serde_json::json!({ "id": w.id, "actual": actual }));
        }
        decoded.push((w.id, bytes));
    }
    if !mismatches.is_empty() {
        return error_with_detail(409, "BlobIdMismatch", serde_json::json!({ "mismatches": mismatches }));
    }

    let store = state.store.lock().unwrap();
    // New bytes only — dedup against the store and within the batch.
    let mut fresh = std::collections::BTreeSet::new();
    let incoming: u64 = decoded
        .iter()
        .filter(|(id, _)| !store.has_blob(id) && fresh.insert(id.as_str()))
        .map(|(_, b)| b.len() as u64)
        .sum();
    if let Some(l) = limits {
        if incoming > 0 {
            let used = match store.blob_bytes_used() {
                Ok(u) => u,
                Err(e) => return error_response(500, format!("measuring blob usage: {e}")),
            };
            if used.saturating_add(incoming) > l.store_quota_bytes {
                return error_with_detail(507, "BlobQuotaExceeded", serde_json::json!({
                    "used": used,
                    "incoming": incoming,
                    "store_quota_bytes": l.store_quota_bytes,
                }));
            }
        }
    }
    let received = decoded.len();
    for (id, bytes) in &decoded {
        if let Err(e) = store.put_blob_bytes(bytes) {
            return error_response(500, format!("put blob {id}: {e}"));
        }
    }
    json_response(200, &serde_json::json!({
        "received": received,
        "added": fresh.len(),
        "skipped": received - fresh.len(),
    }))
}

/// `POST /v1/blobs/fetch` — `{ "ids": [...] }` → `{ "blobs": [{ id,
/// data_b64 }] }` for the ids present (missing ones are omitted; the caller
/// reconciles). A client chunks its requests by the manifest's `size`s.
pub(crate) fn blobs_fetch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    use base64::Engine as _;
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut blobs = Vec::new();
    for id in ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        match store.get_blob_bytes(&id) {
            Ok(bytes) => blobs.push(WireBlob { data_b64: b64().encode(bytes), id }),
            Err(lex_store::StoreError::UnknownBlob(_)) => {}
            Err(e) => return error_response(500, format!("read blob {id}: {e}")),
        }
    }
    json_response(200, &serde_json::json!({ "blobs": blobs }))
}

/// Map a `validate_set_files` failure for the files set at `op_id`.
fn set_files_error(op_id: &str, manifest: &str, e: StoreError) -> Response<Cursor<Vec<u8>>> {
    match e {
        StoreError::MissingBlobs(ids) => error_with_detail(422, "MissingBlobs", serde_json::json!({
            "op_id": op_id,
            "ids": ids,
        })),
        StoreError::InvalidManifest(m) => invalid_manifest(op_id, manifest, m.to_string()),
        other => error_response(500, format!("validating manifest {manifest} of {op_id}: {other}")),
    }
}

fn invalid_manifest(op_id: &str, manifest: &str, reason: String) -> Response<Cursor<Vec<u8>>> {
    error_with_detail(422, "InvalidManifest", serde_json::json!({
        "op_id": op_id,
        "manifest": manifest,
        "reason": reason,
    }))
}

/// Server-side gate for a pushed `SetFiles` record (#1007), run by
/// `/v1/ops/batch` before anything is persisted. A non-`SetFiles` record
/// passes. A `SetFiles` must record `FilesOnly` (anything else would let a
/// files op rewrite the sig→stage map), name a canonical, valid manifest
/// this store holds together with every entry blob at its stated size, and
/// stay within `blob_limits.max_manifest_entries`.
pub(crate) fn check_set_files(
    store: &Store,
    limits: Option<BlobLimits>,
    rec: &OperationRecord,
) -> Result<(), Response<Cursor<Vec<u8>>>> {
    let OperationKind::SetFiles { manifest } = &rec.op.kind else {
        return Ok(());
    };
    if !matches!(rec.produces, StageTransition::FilesOnly) {
        return Err(error_with_detail(422, "InvalidTransition", serde_json::json!({
            "op_id": rec.op_id,
            "reason": "a set_files op must produce files_only",
        })));
    }
    let m = store
        .validate_set_files(manifest)
        .map_err(|e| set_files_error(&rec.op_id, manifest, e))?;
    if let Some(l) = limits {
        if m.entries.len() > l.max_manifest_entries {
            return Err(invalid_manifest(&rec.op_id, manifest, format!(
                "{} entries exceeds the limit of {}",
                m.entries.len(),
                l.max_manifest_entries
            )));
        }
    }
    Ok(())
}

/// Gate for advancing a branch head to `head_op` (#1007): the files in force
/// there must be well-defined and complete. `Ambiguous` (a merge of
/// disagreeing manifests with no `SetFiles` on top) → 422
/// `AmbiguousManifest`; a manifest whose closure isn't here → 422
/// `MissingBlobs` / `InvalidManifest`. No files, or an op this store doesn't
/// know (the advance itself answers that), passes.
pub(crate) fn check_head_files(store: &Store, head_op: &str) -> Result<(), Response<Cursor<Vec<u8>>>> {
    match store.manifest_at(head_op) {
        Ok(ManifestAt::Absent) | Err(StoreError::UnknownOp(_)) => Ok(()),
        Ok(ManifestAt::Ambiguous) => Err(error_with_detail(422, "AmbiguousManifest", serde_json::json!({
            "head_op": head_op,
            "reason": "the head merges histories with different files manifests; \
                       append a set_files op recording the merged manifest first",
        }))),
        Ok(ManifestAt::Set { manifest }) => store
            .validate_set_files(&manifest)
            .map(|_| ())
            .map_err(|e| set_files_error(head_op, &manifest, e)),
        Err(e) => Err(error_response(500, format!("manifest_at {head_op}: {e}"))),
    }
}

/// Parse a `{ "ids": [...] }` body into a `Vec<String>`.
fn parse_ids(body: &str) -> Result<Vec<String>, Response<Cursor<Vec<u8>>>> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| error_response(400, format!("body must be JSON: {e}")))?;
    let ids = v.get("ids").and_then(|i| i.as_array())
        .ok_or_else(|| error_response(400, "missing array field `ids`"))?;
    Ok(ids.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}
