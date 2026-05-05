//! Request routing for the agent API.
//!
//! Each handler is a synchronous function that returns
//! `Result<serde_json::Value, ApiError>`. The dispatcher wraps the result
//! in an HTTP response — successes as 200 with the JSON body, structured
//! errors as 4xx/5xx with a JSON envelope.

use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_store::Store;
use lex_syntax::load_program_from_str;
use lex_vcs::{MergeSession, MergeSessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response};

pub struct State {
    pub store: Mutex<Store>,
    /// In-memory merge sessions, keyed by MergeSessionId. Sessions
    /// are ephemeral by design (#134 foundation): they live for the
    /// lifetime of the server process and are GC'd on commit. A
    /// future slice can persist them to disk so a session survives
    /// process restarts. For now an agent that gets unlucky with a
    /// restart re-runs `merge/start` and gets a fresh session.
    pub sessions: Mutex<HashMap<MergeSessionId, MergeSession>>,
}

impl State {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            store: Mutex::new(Store::open(root)?),
            sessions: Mutex::new(HashMap::new()),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ErrorEnvelope {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<serde_json::Value>,
}

fn json_response(status: u16, body: &serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(bytes)
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

fn error_response(status: u16, msg: impl Into<String>) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(status, &serde_json::to_value(ErrorEnvelope {
        error: msg.into(), detail: None,
    }).unwrap())
}

fn error_with_detail(status: u16, msg: impl Into<String>, detail: serde_json::Value)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    json_response(status, &serde_json::to_value(ErrorEnvelope {
        error: msg.into(), detail: Some(detail),
    }).unwrap())
}

pub fn handle(state: Arc<State>, mut req: Request) -> std::io::Result<()> {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();

    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);

    let resp = route(&state, &method, &path, &query, &body);
    req.respond(resp)
}

fn route(
    state: &State,
    method: &Method,
    path: &str,
    query: &str,
    body: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    match (method, path) {
        (Method::Get, "/v1/health") => json_response(200, &serde_json::json!({"ok": true})),
        (Method::Post, "/v1/parse") => parse_handler(body),
        (Method::Post, "/v1/check") => check_handler(body),
        (Method::Post, "/v1/publish") => publish_handler(state, body),
        (Method::Post, "/v1/patch") => patch_handler(state, body),
        (Method::Get, p) if p.starts_with("/v1/stage/") => {
            let suffix = &p["/v1/stage/".len()..];
            // Match `/v1/stage/<id>/attestations` first so a literal
            // stage_id of "attestations" can't be misrouted.
            if let Some(id) = suffix.strip_suffix("/attestations") {
                stage_attestations_handler(state, id)
            } else {
                stage_handler(state, suffix)
            }
        }
        (Method::Post, "/v1/run") => run_handler(state, body, false),
        (Method::Post, "/v1/replay") => run_handler(state, body, true),
        (Method::Get, p) if p.starts_with("/v1/trace/") => {
            let id = &p["/v1/trace/".len()..];
            trace_handler(state, id)
        }
        (Method::Get, "/v1/diff") => diff_handler(state, query),
        (Method::Post, "/v1/merge/start") => merge_start_handler(state, body),
        (Method::Post, p) if p.starts_with("/v1/merge/") && p.ends_with("/resolve") => {
            let id = &p["/v1/merge/".len()..p.len() - "/resolve".len()];
            merge_resolve_handler(state, id, body)
        }
        _ => error_response(404, format!("unknown route: {method:?} {path}")),
    }
}

#[derive(Deserialize)]
struct ParseReq { source: String }

fn parse_handler(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: ParseReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    match load_program_from_str(&req.source) {
        Ok(prog) => {
            let stages = canonicalize_program(&prog);
            json_response(200, &serde_json::to_value(&stages).unwrap())
        }
        Err(e) => error_response(400, format!("syntax error: {e}")),
    }
}

fn check_handler(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: ParseReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match load_program_from_str(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    let stages = canonicalize_program(&prog);
    match lex_types::check_program(&stages) {
        Ok(_) => json_response(200, &serde_json::json!({"ok": true})),
        Err(errs) => json_response(422, &serde_json::to_value(&errs).unwrap()),
    }
}

#[derive(Deserialize)]
struct PublishReq { source: String, #[serde(default)] activate: bool }

fn publish_handler(state: &State, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: PublishReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match load_program_from_str(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap());
    }

    let store = state.store.lock().unwrap();
    let branch = store.current_branch();

    // Compute diff between what's already on the branch and the new program.
    let old_head = match store.branch_head(&branch) {
        Ok(h) => h,
        Err(e) => return error_response(500, format!("branch_head: {e}")),
    };
    let old_fns: std::collections::BTreeMap<String, lex_ast::FnDecl> = old_head.values()
        .filter_map(|stg| store.get_ast(stg).ok())
        .filter_map(|s| match s {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd)),
            _ => None,
        })
        .collect();
    let new_fns: std::collections::BTreeMap<String, lex_ast::FnDecl> = stages.iter()
        .filter_map(|s| match s {
            lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
            _ => None,
        })
        .collect();
    let report = lex_vcs::compute_diff(&old_fns, &new_fns, false);

    // Build new imports map from any Import stages in the source.
    let mut new_imports: lex_vcs::ImportMap = lex_vcs::ImportMap::new();
    {
        let entry = new_imports.entry("<source>".into()).or_default();
        for s in &stages {
            if let lex_ast::Stage::Import(im) = s {
                entry.insert(im.reference.clone());
            }
        }
    }

    match store.publish_program(&branch, &stages, &report, &new_imports, req.activate) {
        Ok(outcome) => json_response(200, &serde_json::json!({
            "ops": outcome.ops,
            "head_op": outcome.head_op,
        })),
        // The store-write gate (#130) also type-checks at the top
        // of `publish_program`. The handler above already pre-checks,
        // so this branch is reached only on a race or a state we
        // didn't see at handler time. Surface the structured
        // envelope (422) instead of a generic 500 — same shape the
        // initial pre-check uses, so a client only has one error
        // contract to handle.
        Err(lex_store::StoreError::TypeError(errs)) => {
            error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap())
        }
        Err(e) => error_response(500, format!("publish_program: {e}")),
    }
}

#[derive(Deserialize)]
struct PatchReq {
    stage_id: String,
    patch: lex_ast::Patch,
    #[serde(default)] activate: bool,
}

/// POST /v1/patch — apply a structured edit to a stored stage's
/// canonical AST, type-check the result, and publish a new stage.
fn patch_handler(state: &State, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: PatchReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let store = state.store.lock().unwrap();

    // 1. Load.
    let original = match store.get_ast(&req.stage_id) {
        Ok(s) => s, Err(e) => return error_response(404, format!("stage: {e}")),
    };

    // 2. Apply.
    let patched = match lex_ast::apply_patch(&original, &req.patch) {
        Ok(s) => s,
        Err(e) => return error_with_detail(422, "patch failed",
            serde_json::to_value(&e).unwrap_or_default()),
    };

    // 3. Type-check the new stage in isolation.
    let stages = vec![patched.clone()];
    if let Err(errs) = lex_types::check_program(&stages) {
        return error_with_detail(422, "type errors after patch",
            serde_json::to_value(&errs).unwrap_or_default());
    }

    // Routing through apply_operation so /v1/patch participates in
    // the op DAG. We know this op is always a body change on the
    // existing sig (a patch can't add a brand-new fn).
    let branch = store.current_branch();

    // Find the sig — patched stage's sig must match the original's.
    let sig = match lex_ast::sig_id(&patched) {
        Some(s) => s,
        None => return error_response(500, "patched stage has no sig_id"),
    };

    let new_id = match store.publish(&patched) {
        Ok(id) => id, Err(e) => return error_response(500, format!("publish: {e}")),
    };
    if req.activate {
        if let Err(e) = store.activate(&new_id) {
            return error_response(500, format!("activate: {e}"));
        }
    }

    // Determine op kind: ChangeEffectSig if effects differ, ModifyBody otherwise.
    let original_effects: std::collections::BTreeSet<String> = match &original {
        lex_ast::Stage::FnDecl(fd) => fd.effects.iter().map(|e| e.name.clone()).collect(),
        _ => std::collections::BTreeSet::new(),
    };
    let patched_effects: std::collections::BTreeSet<String> = match &patched {
        lex_ast::Stage::FnDecl(fd) => fd.effects.iter().map(|e| e.name.clone()).collect(),
        _ => std::collections::BTreeSet::new(),
    };
    let head_now = match store.get_branch(&branch) {
        Ok(b) => b.and_then(|b| b.head_op),
        Err(e) => return error_response(500, format!("get_branch: {e}")),
    };
    let kind = if original_effects != patched_effects {
        lex_vcs::OperationKind::ChangeEffectSig {
            sig_id: sig.clone(),
            from_stage_id: req.stage_id.clone(),
            to_stage_id: new_id.clone(),
            from_effects: original_effects,
            to_effects: patched_effects,
        }
    } else {
        lex_vcs::OperationKind::ModifyBody {
            sig_id: sig.clone(),
            from_stage_id: req.stage_id.clone(),
            to_stage_id: new_id.clone(),
        }
    };
    let transition = lex_vcs::StageTransition::Replace {
        sig_id: sig.clone(),
        from: req.stage_id.clone(),
        to: new_id.clone(),
    };
    let op = lex_vcs::Operation::new(
        kind,
        head_now.into_iter().collect::<Vec<_>>(),
    );
    let op_id = match store.apply_operation(&branch, op, transition) {
        Ok(id) => id,
        Err(e) => return error_response(500, format!("apply_operation: {e}")),
    };

    let status = format!("{:?}",
        store.get_status(&new_id).unwrap_or(lex_store::StageStatus::Draft)).to_lowercase();
    json_response(200, &serde_json::json!({
        "old_stage_id": req.stage_id,
        "new_stage_id": new_id,
        "sig_id": sig,
        "status": status,
        "op_id": op_id,
    }))
}

fn stage_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    let meta = match store.get_metadata(id) {
        Ok(m) => m, Err(e) => return error_response(404, format!("{e}")),
    };
    let ast = match store.get_ast(id) {
        Ok(a) => a, Err(e) => return error_response(404, format!("{e}")),
    };
    let status = format!("{:?}", store.get_status(id).unwrap_or(lex_store::StageStatus::Draft)).to_lowercase();
    json_response(200, &serde_json::json!({
        "metadata": meta,
        "ast": ast,
        "status": status,
    }))
}

/// `GET /v1/stage/<id>/attestations` — every persisted attestation
/// for this stage, newest-first by timestamp. Issue #132's
/// queryable-evidence consumer surface.
///
/// 404s on unknown stage_id (matches `/v1/stage/<id>`'s shape so a
/// caller round-tripping both endpoints sees consistent errors).
/// Empty list (200) is *evidence of absence*: the stage exists but
/// no producer has attested it.
fn stage_attestations_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    if let Err(e) = store.get_metadata(id) {
        return error_response(404, format!("{e}"));
    }
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("attestation log: {e}")),
    };
    let mut listing = match log.list_for_stage(&id.to_string()) {
        Ok(v) => v,
        Err(e) => return error_response(500, format!("list_for_stage: {e}")),
    };
    listing.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
    json_response(200, &serde_json::json!({"attestations": listing}))
}

#[derive(Deserialize, Default)]
struct PolicyJson {
    #[serde(default)] allow_effects: Vec<String>,
    #[serde(default)] allow_fs_read: Vec<String>,
    #[serde(default)] allow_fs_write: Vec<String>,
    #[serde(default)] budget: Option<u64>,
}

impl PolicyJson {
    fn into_policy(self) -> Policy {
        Policy {
            allow_effects: self.allow_effects.into_iter().collect::<BTreeSet<_>>(),
            allow_fs_read: self.allow_fs_read.into_iter().map(PathBuf::from).collect(),
            allow_fs_write: self.allow_fs_write.into_iter().map(PathBuf::from).collect(),
            allow_net_host: Vec::new(),
            allow_proc: Vec::new(),
            budget: self.budget,
        }
    }
}

#[derive(Deserialize)]
struct RunReq {
    source: String,
    #[serde(rename = "fn")] func: String,
    #[serde(default)] args: Vec<serde_json::Value>,
    #[serde(default)] policy: PolicyJson,
    #[serde(default)] overrides: IndexMap<String, serde_json::Value>,
}

fn run_handler(state: &State, body: &str, with_overrides: bool) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: RunReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match load_program_from_str(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap());
    }
    let bc = compile_program(&stages);
    let policy = req.policy.into_policy();
    if let Err(violations) = check_policy(&bc, &policy) {
        return error_with_detail(403, "policy violation", serde_json::to_value(&violations).unwrap());
    }

    let mut recorder = lex_trace::Recorder::new();
    if with_overrides && !req.overrides.is_empty() {
        recorder = recorder.with_overrides(req.overrides);
    }
    let handle = recorder.handle();
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.set_tracer(Box::new(recorder));

    let vargs: Vec<Value> = req.args.iter().map(json_to_value).collect();
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let result = vm.call(&req.func, vargs);
    let ended = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    let store = state.store.lock().unwrap();
    let (root_out, root_err, status) = match &result {
        Ok(v) => (Some(value_to_json(v)), None, 200u16),
        Err(e) => (None, Some(format!("{e}")), 200u16),
    };
    let tree = handle.finalize(req.func.clone(), serde_json::Value::Null,
        root_out.clone(), root_err.clone(), started, ended);
    let run_id = match store.save_trace(&tree) {
        Ok(id) => id,
        Err(e) => return error_response(500, format!("save_trace: {e}")),
    };

    let mut body = serde_json::json!({
        "run_id": run_id,
        "output": root_out,
    });
    if let Some(err) = root_err {
        body["error"] = serde_json::Value::String(err);
    }
    json_response(status, &body)
}

fn trace_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let store = state.store.lock().unwrap();
    match store.load_trace(id) {
        Ok(t) => json_response(200, &serde_json::to_value(&t).unwrap()),
        Err(e) => error_response(404, format!("{e}")),
    }
}

fn diff_handler(state: &State, query: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut a = None;
    let mut b = None;
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k { "a" => a = Some(v.to_string()), "b" => b = Some(v.to_string()), _ => {} }
        }
    }
    let (Some(a), Some(b)) = (a, b) else {
        return error_response(400, "missing a or b query params");
    };
    let store = state.store.lock().unwrap();
    let ta = match store.load_trace(&a) { Ok(t) => t, Err(e) => return error_response(404, format!("a: {e}")) };
    let tb = match store.load_trace(&b) { Ok(t) => t, Err(e) => return error_response(404, format!("b: {e}")) };
    match lex_trace::diff_runs(&ta, &tb) {
        Some(d) => json_response(200, &serde_json::to_value(&d).unwrap()),
        None => json_response(200, &serde_json::json!({"divergence": null})),
    }
}

fn json_to_value(v: &serde_json::Value) -> Value { Value::from_json(v) }

fn value_to_json(v: &Value) -> serde_json::Value { v.to_json() }

#[derive(Deserialize)]
struct MergeStartReq {
    src_branch: String,
    dst_branch: String,
}

/// `POST /v1/merge/start` (#134) — open a stateful merge between two
/// branch heads and return the conflicts the agent needs to
/// resolve. Auto-resolved sigs (one-sided changes, identical
/// changes both sides) are returned for audit but don't block
/// commit.
///
/// Response: `{ merge_id, src_head, dst_head, lca, conflicts,
/// auto_resolved_count }`. The session is held in process memory
/// keyed by `merge_id` for subsequent `resolve` / `commit` calls
/// (next slices).
fn merge_start_handler(state: &State, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: MergeStartReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let store = state.store.lock().unwrap();
    let src_head = match store.get_branch(&req.src_branch) {
        Ok(Some(b)) => b.head_op,
        Ok(None) => return error_response(404, format!("unknown src branch `{}`", req.src_branch)),
        Err(e) => return error_response(500, format!("src branch read: {e}")),
    };
    let dst_head = match store.get_branch(&req.dst_branch) {
        Ok(Some(b)) => b.head_op,
        Ok(None) => return error_response(404, format!("unknown dst branch `{}`", req.dst_branch)),
        Err(e) => return error_response(500, format!("dst branch read: {e}")),
    };
    let log = match lex_vcs::OpLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("op log: {e}")),
    };
    // Caller doesn't choose merge_ids — minted server-side from
    // wall clock + a per-process counter avoids leaking session
    // ids' shape into the public surface.
    let merge_id = mint_merge_id();
    let session = match MergeSession::start(
        merge_id.clone(),
        &log,
        src_head.as_ref(),
        dst_head.as_ref(),
    ) {
        Ok(s) => s,
        Err(e) => return error_response(500, format!("merge start: {e}")),
    };
    let conflicts: Vec<&lex_vcs::ConflictRecord> = session.remaining_conflicts();
    let auto_resolved_count = session.auto_resolved.len();
    let body = serde_json::json!({
        "merge_id": merge_id,
        "src_head": session.src_head,
        "dst_head": session.dst_head,
        "lca":      session.lca,
        "conflicts": conflicts,
        "auto_resolved_count": auto_resolved_count,
    });
    // Drop the borrow on `conflicts` before moving session into the map.
    drop(conflicts);
    drop(store);
    state.sessions.lock().unwrap().insert(merge_id, session);
    json_response(200, &body)
}

#[derive(Deserialize)]
struct MergeResolveReq {
    /// Each entry is `(conflict_id, resolution)`. The resolution is
    /// the same shape as `lex_vcs::Resolution`'s tagged JSON form
    /// — `{"kind":"take_ours"}`, `{"kind":"take_theirs"}`,
    /// `{"kind":"defer"}`, or `{"kind":"custom","op":{...}}`.
    resolutions: Vec<MergeResolveEntry>,
}

#[derive(Deserialize)]
struct MergeResolveEntry {
    conflict_id: String,
    resolution: lex_vcs::Resolution,
}

/// `POST /v1/merge/<id>/resolve` (#134) — submit batched
/// resolutions against the conflicts surfaced by `merge/start`.
/// Returns one verdict per input: accepted (recorded against the
/// session) or rejected (with structured reason). The session
/// stays alive across calls so an agent can iterate.
///
/// Errors:
/// - 404 if `merge_id` doesn't refer to a live session (a typo
///   or a session GC'd by a server restart).
/// - 400 on malformed body.
fn merge_resolve_handler(
    state: &State,
    merge_id: &str,
    body: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: MergeResolveReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let mut sessions = state.sessions.lock().unwrap();
    let Some(session) = sessions.get_mut(merge_id) else {
        return error_response(404, format!("unknown merge_id `{merge_id}`"));
    };
    let pairs: Vec<(String, lex_vcs::Resolution)> = req.resolutions.into_iter()
        .map(|e| (e.conflict_id, e.resolution))
        .collect();
    let verdicts = session.resolve(pairs);
    let remaining: Vec<&lex_vcs::ConflictRecord> = session.remaining_conflicts();
    let body = serde_json::json!({
        "verdicts": verdicts,
        "remaining_conflicts": remaining,
    });
    json_response(200, &body)
}

fn mint_merge_id() -> MergeSessionId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("merge_{nanos:x}_{n:x}")
}

