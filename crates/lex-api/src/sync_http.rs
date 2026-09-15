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
use lex_vcs::{Intent, IntentLog};

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

/// Parse a `{ "ids": [...] }` body into a `Vec<String>`.
fn parse_ids(body: &str) -> Result<Vec<String>, Response<Cursor<Vec<u8>>>> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| error_response(400, format!("body must be JSON: {e}")))?;
    let ids = v.get("ids").and_then(|i| i.as_array())
        .ok_or_else(|| error_response(400, "missing array field `ids`"))?;
    Ok(ids.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}
