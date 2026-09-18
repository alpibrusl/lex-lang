//! The human review surface over HTTP (lex-hub#92, first slice).
//!
//! In an agent-native VCS humans are not the gate — agents produce and verify
//! continuously. The human's job is to **adjudicate exceptions** and **arbitrate
//! intent**: not to read every diff, but to look at the changes a machine gate
//! couldn't close, see *why* each was made (its recorded Intent), and record a
//! verdict. Those verdicts are `Review` attestations — a human decision lives in
//! the same typed attestation graph as `lex-hub-ci`'s TypeCheck or a Replay, and
//! the same gates consume it (a standing Reject blocks promotion).
//!
//! Two endpoints, tenant-scoped (auth + store selection live in lex-hub):
//!   * `GET  /v1/review/inbox[?branch=<b>]` — the head's stages, each with its
//!     intent and current review state; `needs_review` flags the exceptions.
//!   * `POST /v1/review/verdict` — record an Approve / Reject / RequestChanges
//!     verdict as a `Review` attestation.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Cursor;
use tiny_http::Response;

use crate::handlers::{error_response, json_response, State};

/// `GET /v1/review/inbox[?branch=<name>]` — the review inbox for a branch head:
/// each head stage with its declaration name, the intent it was made under (the
/// *why*), its latest review verdict, and whether it still needs a human look.
pub(crate) fn review_inbox_handler(state: &State, query: &str) -> Response<Cursor<Vec<u8>>> {
    let branch = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("branch="))
        .map(str::to_string);
    let store = state.store.lock().unwrap();
    let branch = branch.unwrap_or_else(|| store.current_branch());

    let head_op = match store.get_branch(&branch) {
        Ok(Some(b)) => b.head_op,
        Ok(None) => return error_response(404, format!("unknown branch {branch:?}")),
        Err(e) => return error_response(500, format!("get_branch: {e}")),
    };
    let Some(head_op) = head_op else {
        return json_response(200, &serde_json::json!({ "branch": branch, "items": [] }));
    };

    // stage_id → intent prompt (first line), from the op that produced it.
    let stage_intent = match stage_intents(&store, &head_op) {
        Ok(m) => m,
        Err(e) => return error_response(500, format!("reading intents: {e}")),
    };

    // The head's stages, read per-SigId so distinct names survive a shared StageId.
    let head = store.branch_head(&branch).unwrap_or_default();
    let pairs: Vec<(String, String)> = head.iter().map(|(s, st)| (s.clone(), st.clone())).collect();
    let asts = store.get_asts_for_sigs_bulk(&pairs);

    let mut items = Vec::new();
    for ((_sig, stage_id), ast) in pairs.iter().zip(asts) {
        let name = match ast {
            Ok(lex_ast::Stage::FnDecl(fd)) => fd.name,
            Ok(lex_ast::Stage::TypeDecl(td)) => td.name,
            _ => continue,
        };
        let verdict = store
            .latest_review_verdict(stage_id)
            .ok()
            .flatten()
            .map(|v| match v {
                lex_vcs::ReviewVerdict::Approve => "approved",
                lex_vcs::ReviewVerdict::Reject => "rejected",
                lex_vcs::ReviewVerdict::RequestChanges => "changes_requested",
            })
            .unwrap_or("none");
        // The exception rule: a stage needs a human unless it is already
        // approved. Unreviewed, rejected, and changes-requested all surface.
        let needs_review = verdict != "approved";
        items.push(serde_json::json!({
            "stage_id": stage_id,
            "name": name,
            "intent": stage_intent.get(stage_id),
            "review": verdict,
            "needs_review": needs_review,
        }));
    }

    json_response(200, &serde_json::json!({
        "branch": branch,
        "head_op": head_op,
        "items": items,
    }))
}

#[derive(Deserialize)]
struct VerdictReq {
    stage_id: String,
    /// "approve" | "reject" | "request_changes".
    verdict: String,
    reviewer: String,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /v1/review/verdict` — record a human review verdict as a `Review`
/// attestation on a stage. The verdict enters the same attestation graph the
/// gates consume (a Reject blocks promotion via `latest_review_verdict`).
pub(crate) fn review_verdict_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let req: VerdictReq = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let verdict = match req.verdict.as_str() {
        "approve" => lex_vcs::ReviewVerdict::Approve,
        "reject" => lex_vcs::ReviewVerdict::Reject,
        "request_changes" => lex_vcs::ReviewVerdict::RequestChanges,
        other => return error_response(400, format!("verdict must be approve|reject|request_changes, got {other:?}")),
    };
    if req.reviewer.trim().is_empty() {
        return error_response(400, "reviewer must be non-empty");
    }
    let store = state.store.lock().unwrap();
    // Guard: the stage must exist, so a verdict can't be filed on a typo.
    if store.get_metadata(&req.stage_id).is_err() {
        return error_response(404, format!("unknown stage {:?}", req.stage_id));
    }
    match store.record_review(&req.stage_id, None, &req.reviewer, verdict, req.note) {
        Ok(id) => json_response(201, &serde_json::json!({
            "attestation_id": id,
            "stage_id": req.stage_id,
            "reviewer": req.reviewer,
        })),
        Err(e) => error_response(500, format!("record_review: {e}")),
    }
}

/// Map each head stage to the intent prompt (first line) of the op that
/// produced it, by walking the op log and joining to the intent log.
fn stage_intents(
    store: &lex_store::Store,
    head_op: &str,
) -> Result<BTreeMap<String, String>, String> {
    let log = lex_vcs::OpLog::open(store.root()).map_err(|e| e.to_string())?;
    let intents = lex_vcs::IntentLog::open(store.root()).map_err(|e| e.to_string())?;
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for rec in log.walk_forward(&head_op.to_string(), None).map_err(|e| e.to_string())? {
        let Some(intent_id) = &rec.op.intent_id else { continue };
        let Some(intent) = intents.get(intent_id).map_err(|e| e.to_string())? else { continue };
        let first_line = intent.prompt.lines().next().unwrap_or("").to_string();
        for stage_id in rec.produces.stage_ids() {
            out.insert(stage_id, first_line.clone());
        }
    }
    Ok(out)
}
