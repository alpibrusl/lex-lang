//! Branch management over HTTP (#839 follow-up). Extracted from
//! `handlers.rs` to keep that file within the source line budget.
//!
//! Lets a remote client create and switch branches — not just probe
//! heads — so a two-branch divergence, and therefore the merge gates,
//! can be driven end to end over the API instead of only through the
//! store/CLI. The tenant scoping and auth live one layer up (lex-hub);
//! these operate on the request's already-scoped `State::store`.

use serde::Deserialize;
use std::io::Cursor;
use tiny_http::Response;

use crate::handlers::{error_response, error_with_detail, json_response, State};

/// `GET /v1/branches/<name>/head` — probe the branch head. Returns
/// `{ "head_op": Option<OpId> }`; the delta-probe half of `op push`.
pub(crate) fn branch_head_handler(state: &State, name: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let head = match store.get_branch(name) {
        Ok(Some(b)) => b.head_op,
        Ok(None) => None,
        Err(e) => return error_response(500, format!("get_branch: {e}")),
    };
    json_response(200, &serde_json::json!({ "branch": name, "head_op": head }))
}

/// `POST /v1/branches/<name>/head` — advance a branch head, the ref half
/// of `op push`. Body `{ "head_op": "<op_id>" }`. Fast-forward only: a
/// non-fast-forward is refused with 409 (git-style), so a disjoint or
/// diverged push can't clobber a shared branch. The op objects must
/// already be present (the ops batch runs first); an unknown `head_op`
/// reads as a non-fast-forward against a head it can't reach.
pub(crate) fn branch_advance_head_handler(state: &State, name: &str, body: &str)
    -> Response<Cursor<Vec<u8>>>
{
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return error_response(400, format!("body must be JSON: {e}")),
    };
    let head_op = match v.get("head_op").and_then(|h| h.as_str()) {
        Some(s) => s.to_string(),
        None => return error_response(400, "missing string field `head_op`"),
    };
    let store = state.store.lock().unwrap();
    // The head before the advance — the ops between it and the new head are
    // the ones the hosted CI runner (#93) attests.
    let prev_head = store.get_branch(name).ok().flatten().and_then(|b| b.head_op);
    match store.advance_branch_head_ff(name, &head_op) {
        Ok(advance) => {
            // #93 hosted CI: independently re-run the type-check gate on the
            // new head and record a `lex-hub-ci`-produced TypeCheck
            // attestation, so `require-attestation` gates are backed by a
            // trusted server-side producer, not the pushing client. A failure
            // here doesn't undo the (already-committed, fast-forward) advance;
            // it's recorded as `TypeCheck::Failed` and surfaced in `ci`.
            let ci = store
                .verify_head_and_attest(name, prev_head.as_deref(), &head_op)
                .ok();
            json_response(200, &serde_json::json!({
                "branch": name,
                "head_op": head_op,
                "advance": advance,
                "ci": ci,
            }))
        }
        Err(lex_store::StoreError::NonFastForward { branch, current, attempted }) =>
            error_with_detail(409, "NonFastForward", serde_json::json!({
                "branch": branch,
                "current": current,
                "attempted": attempted,
            })),
        Err(e) => error_response(500, format!("advance_branch_head_ff: {e}")),
    }
}

/// `GET /v1/branches` — list branches and the current one.
pub(crate) fn branches_list_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let branches = match store.list_branches() {
        Ok(b) => b,
        Err(e) => return error_response(500, format!("list_branches: {e}")),
    };
    json_response(200, &serde_json::json!({
        "branches": branches,
        "current": store.current_branch(),
    }))
}

#[derive(Deserialize)]
struct BranchCreateReq {
    name: String,
    /// Branch to fork from; defaults to the current branch.
    #[serde(default)]
    from: Option<String>,
    /// Switch the current branch to the new one after creating it.
    #[serde(default)]
    checkout: bool,
}

/// `POST /v1/branches` — create a branch. Body: `{ name, from?,
/// checkout? }`. `from` defaults to the current branch; `checkout`
/// switches to the new branch on success. Response: `{ name, from,
/// head_op, current }`.
///
/// 400 on a malformed body or a rejected/duplicate name; 404 when
/// `from` doesn't exist.
pub(crate) fn branch_create_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let req: BranchCreateReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let store = state.store.lock().unwrap();
    let from = req.from.unwrap_or_else(|| store.current_branch());
    // A non-default `from` must exist (create_branch reads its head).
    if from != lex_store::DEFAULT_BRANCH && matches!(store.get_branch(&from), Ok(None)) {
        return error_response(404, format!("unknown source branch `{from}`"));
    }
    if let Err(e) = store.create_branch(&req.name, &from) {
        // Name rejected or already exists → 400; anything else → 500.
        return match e {
            lex_store::StoreError::InvalidTransition(msg) => error_response(400, msg),
            other => error_response(500, format!("create_branch: {other}")),
        };
    }
    if req.checkout {
        if let Err(e) = store.set_current_branch(&req.name) {
            return error_response(500, format!("checkout after create: {e}"));
        }
    }
    let head = store.get_branch(&req.name).ok().flatten().and_then(|b| b.head_op);
    json_response(201, &serde_json::json!({
        "name": req.name,
        "from": from,
        "head_op": head,
        "current": store.current_branch(),
    }))
}

/// `POST /v1/branches/<name>/checkout` — set the current branch.
/// Response: `{ "current": name, "head_op": Option<OpId> }`.
/// 404 when the branch doesn't exist.
pub(crate) fn branch_checkout_handler(state: &State, name: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    if let Err(e) = store.set_current_branch(name) {
        return match e {
            lex_store::StoreError::UnknownBranch(b) => error_response(404, format!("unknown branch `{b}`")),
            other => error_response(500, format!("set_current_branch: {other}")),
        };
    }
    let head = store.get_branch(name).ok().flatten().and_then(|b| b.head_op);
    json_response(200, &serde_json::json!({
        "current": name,
        "head_op": head,
    }))
}
