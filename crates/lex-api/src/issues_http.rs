//! Derived issue/project state over HTTP (#949 phase 3).
//!
//! A board is a *view*: `GET /v1/issues` returns every issue with its state
//! computed from the op-log (open / in progress / verified / blocked),
//! `GET /v1/issues/<id>` adds the recorded verdicts, and `GET /v1/projects`
//! groups issues by project with per-state counts. Nothing here is stored or
//! moved by hand — see `lex_store::issues` for the derivation rules — so the
//! board cannot drift from the code. The hub delegates these per tenant; the
//! browser console's owner-auth wrapping is lex-hub's concern.

use std::collections::BTreeMap;
use std::io::Cursor;
use tiny_http::Response;

use crate::handlers::{error_response, json_response, State};
use lex_store::issues::{
    all_issue_status, issue_status, issue_verdicts, issues_in_progress, IssueState, IssueStatus,
};
use lex_vcs::IssueLog;

/// `GET /v1/issues` — every issue with its derived state.
pub fn issues_state_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    match all_issue_status(&store) {
        Ok(issues) => json_response(200, &serde_json::json!({ "issues": issues })),
        Err(e) => error_response(500, format!("deriving issue state: {e}")),
    }
}

/// `GET /v1/issues/<id>` — one issue, its derived state, and every recorded
/// `IssueVerified` verdict. 404 for an unknown id.
pub fn issue_detail_handler(state: &State, id: &str) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let log = match IssueLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening issue log: {e}")),
    };
    let issue = match log.get(&id.to_string()) {
        Ok(Some(i)) => i,
        Ok(None) => return error_response(404, format!("unknown issue `{id}`")),
        Err(e) => return error_response(500, format!("reading issue {id}: {e}")),
    };
    let in_progress = match issues_in_progress(&store) {
        Ok(s) => s,
        Err(e) => return error_response(500, format!("scanning provenance: {e}")),
    };
    let status = match issue_status(&store, &issue, &in_progress) {
        Ok(s) => s,
        Err(e) => return error_response(500, format!("deriving state for {id}: {e}")),
    };
    let verdicts = match issue_verdicts(&store, id) {
        Ok(v) => v,
        Err(e) => return error_response(500, format!("reading verdicts for {id}: {e}")),
    };
    json_response(200, &serde_json::json!({
        "issue": status.issue,
        "state": status.state,
        "blocked_on": status.blocked_on,
        "has_work": status.has_work,
        "verdicts": verdicts,
    }))
}

/// `GET /v1/projects` — issues grouped by project (a project is a subgraph
/// with a goal), each with per-state counts. Issues with no project appear
/// in `/v1/issues` only.
pub fn projects_handler(state: &State) -> Response<Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let all = match all_issue_status(&store) {
        Ok(v) => v,
        Err(e) => return error_response(500, format!("deriving issue state: {e}")),
    };
    let mut by_project: BTreeMap<String, Vec<&IssueStatus>> = BTreeMap::new();
    for s in &all {
        if let Some(p) = &s.issue.project {
            by_project.entry(p.clone()).or_default().push(s);
        }
    }
    let projects: Vec<serde_json::Value> = by_project
        .into_iter()
        .map(|(name, issues)| {
            let count = |st: IssueState| issues.iter().filter(|s| s.state == st).count();
            serde_json::json!({
                "name": name,
                "counts": {
                    "open": count(IssueState::Open),
                    "in_progress": count(IssueState::InProgress),
                    "verified": count(IssueState::Verified),
                    "blocked": count(IssueState::Blocked),
                },
                "issues": issues.iter().map(|s| serde_json::json!({
                    "issue_id": s.issue.issue_id,
                    "title": s.issue.title,
                    "shape": s.issue.acceptance.shape(),
                    "state": s.state,
                    "blocked_on": s.blocked_on,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_response(200, &serde_json::json!({ "projects": projects }))
}
