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

use crate::handlers::{error_response, json_response, State};
use lex_ast::{stage_id, Stage};
use lex_vcs::{Intent, IntentLog, Issue, IssueLog};

/// `POST /v1/stages/batch` — receive stage blobs. Body: a JSON array of
/// `Stage`. Each is stored content-addressed (idempotent); a stage that
/// isn't hashable (an import) is skipped rather than erroring the batch.
pub(crate) fn stages_batch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let stages: Vec<Stage> = match serde_json::from_str(body) {
        Ok(s) => s,
        Err(e) => return error_response(400, format!("body must be a JSON array of Stage: {e}")),
    };
    let store = state.store.lock().unwrap();
    let (mut added, mut skipped) = (0usize, 0usize);
    for stage in &stages {
        let id = match stage_id(stage) {
            Some(id) => id,
            None => { skipped += 1; continue } // import / unhashable
        };
        let existed = store.get_ast(&id).is_ok();
        if let Err(e) = store.publish(stage) {
            return error_response(500, format!("publish stage {id}: {e}"));
        }
        if existed { skipped += 1 } else { added += 1 }
    }
    json_response(200, &serde_json::json!({
        "received": stages.len(), "added": added, "skipped": skipped,
    }))
}

/// `POST /v1/stages/fetch` — return stage blobs for a set of ids. Body:
/// `{ "ids": ["<stage_id>", ...] }`. Returns a JSON array of the `Stage`s
/// present (missing ids are silently omitted; the caller reconciles).
pub(crate) fn stages_fetch_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    let stages: Vec<Stage> = ids.iter().filter_map(|id| store.get_ast(id).ok()).collect();
    json_response(200, &serde_json::json!({ "stages": stages }))
}

/// `POST /v1/stages/missing` — existence check. Body: `{ "ids": ["<stage_id>",
/// …] }`. Returns `{ "missing": [<the ids this store does NOT have>] }`. A cheap
/// companion to `batch`/`fetch`: `op push` uses it to reconcile the full
/// stage-closure of the head it's advancing to — push only the blobs the
/// remote actually lacks — without downloading every stage body just to learn
/// which are present (which `fetch` would force). Idempotent, read-only.
pub(crate) fn stages_missing_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let ids = match parse_ids(body) {
        Ok(ids) => ids,
        Err(resp) => return resp,
    };
    let store = state.store.lock().unwrap();
    let missing: Vec<String> = ids
        .into_iter()
        .filter(|id| store.get_ast(id).is_err())
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

/// Parse a `{ "ids": [...] }` body into a `Vec<String>`.
fn parse_ids(body: &str) -> Result<Vec<String>, Response<Cursor<Vec<u8>>>> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| error_response(400, format!("body must be JSON: {e}")))?;
    let ids = v.get("ids").and_then(|i| i.as_array())
        .ok_or_else(|| error_response(400, "missing array field `ids`"))?;
    Ok(ids.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}
