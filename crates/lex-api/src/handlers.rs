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
    /// Filesystem root of the store. Held alongside the `Store`
    /// itself so handlers that need to read store-level files
    /// (e.g. `users.json` for actor auth) don't have to round-
    /// trip through the lock.
    pub root: PathBuf,
    /// In-memory merge sessions, keyed by MergeSessionId. Sessions
    /// are ephemeral by design (#134 foundation): they live for the
    /// lifetime of the server process and are GC'd on commit. A
    /// future slice can persist them to disk so a session survives
    /// process restarts. For now an agent that gets unlucky with a
    /// restart re-runs `merge/start` and gets a fresh session.
    pub sessions: Mutex<HashMap<MergeSessionId, ApiMergeSession>>,
}

/// Server-side wrapper around [`MergeSession`] carrying the
/// branch names that started the merge. The lex-vcs session
/// itself only tracks `OpId` heads; commit needs the dst branch
/// name to advance the right head, and the src branch name is
/// kept for round-trip auditability ("which branch did we merge
/// from?").
pub struct ApiMergeSession {
    pub inner: MergeSession,
    pub src_branch: String,
    pub dst_branch: String,
}

impl State {
    pub fn open(root: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            store: Mutex::new(Store::open(&root)?),
            root,
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

/// Map a `StoreError` from a write path (`apply_operation` /
/// `apply_operation_checked`) to an HTTP response. The only special
/// case today is `Contention` (#262 multi-writer CAS retries
/// exhausted), which maps to 503 with a `Retry-After` header so
/// clients back off rather than hammering the same branch tip.
fn write_error_response(prefix: &str, err: lex_store::StoreError)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    if let lex_store::StoreError::Contention { branch, attempts } = &err {
        let body = serde_json::to_vec(&ErrorEnvelope {
            error: format!("{prefix}: branch '{branch}' is contended (attempts={attempts})"),
            detail: Some(serde_json::json!({
                "kind": "contention",
                "branch": branch,
                "attempts": attempts,
            })),
        }).unwrap_or_else(|_| b"{}".to_vec());
        return Response::from_data(body)
            .with_status_code(503)
            .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
            .with_header(Header::from_bytes(&b"Retry-After"[..], &b"1"[..]).unwrap());
    }
    error_response(500, format!("{prefix}: {err}"))
}

pub fn handle(state: Arc<State>, mut req: Request) -> std::io::Result<()> {
    let method = req.method().clone();
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();

    // `X-Lex-User` is the v3d session identifier — set by humans
    // operating the web UI through whatever proxy fronts auth, or
    // by AI agents calling the JSON API. We pluck it once here so
    // every handler can take it as a borrowed string.
    let x_lex_user = req.headers().iter()
        .find(|h| h.field.equiv("x-lex-user"))
        .map(|h| h.value.as_str().to_string());

    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);

    let resp = route(&state, &method, &path, &query, &body, x_lex_user.as_deref());
    req.respond(resp)
}

fn route(
    state: &State,
    method: &Method,
    path: &str,
    query: &str,
    body: &str,
    x_lex_user: Option<&str>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    match (method, path) {
        // ---- lex-tea v2 (HTML browser) ------------------------
        (Method::Get, "/") => crate::web::activity_handler(state),
        (Method::Get, "/web/branches") => crate::web::branches_handler(state),
        (Method::Get, "/web/trust") => crate::web::trust_handler(state),
        (Method::Get, "/web/attention") => crate::web::attention_handler(state),
        (Method::Get, p) if p.starts_with("/web/branch/") => {
            let name = &p["/web/branch/".len()..];
            crate::web::branch_handler(state, name)
        }
        (Method::Get, p) if p.starts_with("/web/stage/") => {
            let id = &p["/web/stage/".len()..];
            crate::web::stage_html_handler(state, id)
        }
        // lex-tea v3 human-triage actions (#172). HTML forms post
        // to /web/stage/<id>/{pin,defer,block,unblock} with a
        // `reason` body. All four share one handler; the verb in
        // the path picks the AttestationKind.
        (Method::Post, p) if p.starts_with("/web/stage/") && (
            p.ends_with("/pin") || p.ends_with("/defer")
            || p.ends_with("/block") || p.ends_with("/unblock")
        ) => {
            let prefix_len = "/web/stage/".len();
            let last_slash = p.rfind('/').unwrap_or(p.len());
            let id = &p[prefix_len..last_slash];
            let verb = &p[last_slash + 1..];
            let decision = match verb {
                "pin"     => crate::web::WebStageDecision::Pin,
                "defer"   => crate::web::WebStageDecision::Defer,
                "block"   => crate::web::WebStageDecision::Block,
                "unblock" => crate::web::WebStageDecision::Unblock,
                _ => unreachable!("matched in outer guard"),
            };
            crate::web::stage_decision_handler(state, id, body, decision, x_lex_user)
        }
        // ---- JSON API -----------------------------------------
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
        (Method::Post, p) if p.starts_with("/v1/merge/") && p.ends_with("/commit") => {
            let id = &p["/v1/merge/".len()..p.len() - "/commit".len()];
            merge_commit_handler(state, id)
        }
        // ---- #242: append-only sync of op log + attestation log
        (Method::Post, "/v1/ops/batch") => ops_batch_handler(state, body),
        (Method::Post, "/v1/attestations/batch") => attestations_batch_handler(state, body),
        // Probe endpoint for `lex op push` to discover the remote's
        // current head before computing a delta. Returns
        // `{ "head_op": Option<OpId> }`. `<branch>` is URL-encoded.
        (Method::Get, p) if p.starts_with("/v1/branches/") && p.ends_with("/head") => {
            let name = &p["/v1/branches/".len()..p.len() - "/head".len()];
            branch_head_handler(state, name)
        }
        // ---- #260: append-only fetch (inverse of #242 push)
        // Body is a JSON array of OperationRecords reachable from
        // `branch.head_op` but not from `after`, oldest-first.
        (Method::Get, "/v1/ops/since") => ops_since_handler(state, query),
        (Method::Get, "/v1/attestations/since") => attestations_since_handler(state, query),
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

pub(crate) fn check_handler(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
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

pub(crate) fn publish_handler(state: &State, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let req: PublishReq = match serde_json::from_str(body) {
        Ok(r) => r, Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let prog = match load_program_from_str(&req.source) {
        Ok(p) => p, Err(e) => return error_response(400, format!("syntax error: {e}")),
    };
    // #168: rewrite stdlib parse calls to parse_strict so the
    // bytecode emitted from these stages enforces required-field
    // checks at runtime.
    let mut stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
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
        Err(e) => write_error_response("publish_program", e),
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
        // #247: budget delta is part of the canonical payload now.
        // Patch endpoints don't currently rehydrate the AST to
        // recompute budgets, so leave them None — clients that
        // need budget tracking should publish through the diff
        // pipeline (`lex publish`) where `compute_diff` populates
        // them.
        let from_budget = lex_vcs::operation_budget_from_effects(&original_effects);
        let to_budget = lex_vcs::operation_budget_from_effects(&patched_effects);
        lex_vcs::OperationKind::ChangeEffectSig {
            sig_id: sig.clone(),
            from_stage_id: req.stage_id.clone(),
            to_stage_id: new_id.clone(),
            from_effects: original_effects,
            to_effects: patched_effects,
            from_budget,
            to_budget,
        }
    } else {
        let budget = lex_vcs::operation_budget_from_effects(&original_effects);
        lex_vcs::OperationKind::ModifyBody {
            sig_id: sig.clone(),
            from_stage_id: req.stage_id.clone(),
            to_stage_id: new_id.clone(),
            from_budget: budget,
            to_budget: budget,
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
        Err(e) => return write_error_response("apply_operation", e),
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

pub(crate) fn stage_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
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
pub(crate) fn stage_attestations_handler(state: &State, id: &str) -> Response<std::io::Cursor<Vec<u8>>> {
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

pub(crate) fn run_handler(state: &State, body: &str, with_overrides: bool) -> Response<std::io::Cursor<Vec<u8>>> {
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
    drop(conflicts);
    drop(store);
    let wrapped = ApiMergeSession {
        inner: session,
        src_branch: req.src_branch,
        dst_branch: req.dst_branch,
    };
    state.sessions.lock().unwrap().insert(merge_id, wrapped);
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
    let Some(wrapped) = sessions.get_mut(merge_id) else {
        return error_response(404, format!("unknown merge_id `{merge_id}`"));
    };
    let pairs: Vec<(String, lex_vcs::Resolution)> = req.resolutions.into_iter()
        .map(|e| (e.conflict_id, e.resolution))
        .collect();
    let verdicts = wrapped.inner.resolve(pairs);
    let remaining: Vec<&lex_vcs::ConflictRecord> = wrapped.inner.remaining_conflicts();
    let body = serde_json::json!({
        "verdicts": verdicts,
        "remaining_conflicts": remaining,
    });
    json_response(200, &body)
}

/// `POST /v1/merge/<id>/commit` (#134) — finalize a merge
/// session. Builds a `Merge` op from the auto-resolved sigs +
/// the conflict resolutions, applies it to the dst branch, and
/// returns the new head op id. The session is dropped on
/// success; the caller would re-run `merge/start` to land
/// further changes.
///
/// Errors:
/// - 404: unknown `merge_id`.
/// - 422: conflicts remaining (pass `Defer` or just don't
///   resolve a conflict and you land here). Body carries the
///   list so the caller knows which still need attention.
/// - 422: a `Custom` resolution was used. The data layer
///   supports them but landing them via HTTP needs an extra
///   pass to apply the custom op against the dst branch
///   first; deferred to a follow-up slice. Use TakeOurs /
///   TakeTheirs for now.
/// - 500: filesystem error while landing the merge op.
fn merge_commit_handler(
    state: &State,
    merge_id: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    use std::collections::BTreeMap;
    let wrapped = match state.sessions.lock().unwrap().remove(merge_id) {
        Some(w) => w,
        None => return error_response(404, format!("unknown merge_id `{merge_id}`")),
    };
    let dst_branch = wrapped.dst_branch.clone();
    let src_head = wrapped.inner.src_head.clone();
    let dst_head = wrapped.inner.dst_head.clone();
    let auto_resolved = wrapped.inner.auto_resolved.clone();

    // Translate auto-resolved + resolutions into the StageTransition::Merge
    // entries map. Only sigs whose head changes relative to dst go in.
    let mut entries: BTreeMap<lex_vcs::SigId, Option<lex_vcs::StageId>> = BTreeMap::new();

    // Auto-resolved: only `Src` (one-sided change on src) modifies dst.
    for outcome in &auto_resolved {
        if let lex_vcs::MergeOutcome::Src { sig_id, stage_id } = outcome {
            entries.insert(sig_id.clone(), stage_id.clone());
        }
    }

    // Conflict resolutions.
    let resolved = match wrapped.inner.commit() {
        Ok(r) => r,
        Err(lex_vcs::CommitError::ConflictsRemaining(ids)) => {
            // Re-insert isn't possible since we removed above; the
            // caller will need to re-start. That's acceptable: a
            // commit-with-unresolved-conflicts is operator error.
            return error_with_detail(
                422,
                "conflicts remaining",
                serde_json::json!({"unresolved": ids}),
            );
        }
    };

    for (conflict_id, resolution) in resolved {
        match resolution {
            lex_vcs::Resolution::TakeOurs => {
                // Dst already has its head. No entry needed.
            }
            lex_vcs::Resolution::TakeTheirs => {
                // Find the conflict's `theirs` stage_id in the
                // session snapshot. We don't have direct access to
                // it post-commit (commit consumed the session); but
                // we can reconstruct from `auto_resolved` plus the
                // session's pre-commit conflict map. Since we
                // already moved the inner session, the cleanest fix
                // for this slice is to rebuild from the on-disk
                // graph: walk src_head, find the latest stage for
                // the conflict's sig.
                match resolve_take_theirs(state, &src_head, &conflict_id) {
                    Ok(stage_id) => {
                        entries.insert(conflict_id.clone(), stage_id);
                    }
                    Err(e) => return error_response(500, format!("resolve take_theirs: {e}")),
                }
            }
            lex_vcs::Resolution::Custom { op } => {
                // The agent's brand-new op carries the merge target
                // in its kind (e.g. ModifyBody.to_stage_id). The op
                // itself isn't separately recorded in the log here
                // — its head-map effect is folded into the merge
                // op's entries map. Callers that want the op as a
                // first-class history entry should publish it via
                // /v1/publish first and submit a TakeTheirs/TakeOurs
                // resolution against the resulting head.
                match op.kind.merge_target() {
                    Some((sig, stage)) => {
                        if sig != conflict_id {
                            return error_with_detail(
                                422,
                                "custom op targets a different sig than the conflict",
                                serde_json::json!({
                                    "conflict_id": conflict_id,
                                    "op_targets": sig,
                                }),
                            );
                        }
                        entries.insert(conflict_id, stage);
                    }
                    None => {
                        return error_with_detail(
                            422,
                            "custom op kind doesn't yield a single sig→stage delta",
                            serde_json::json!({
                                "conflict_id": conflict_id,
                                "kind": serde_json::to_value(&op.kind).unwrap_or(serde_json::Value::Null),
                            }),
                        );
                    }
                }
            }
            lex_vcs::Resolution::Defer => {
                // Unreachable: commit() rejects Defer above.
                return error_response(500, "internal: Defer slipped past commit gate");
            }
        }
    }

    let resolved_count = entries.len();
    let mut parents: Vec<lex_vcs::OpId> = Vec::new();
    if let Some(d) = dst_head { parents.push(d); }
    if let Some(s) = src_head { parents.push(s); }
    let op = lex_vcs::Operation::new(
        lex_vcs::OperationKind::Merge { resolved: resolved_count },
        parents,
    );
    let transition = lex_vcs::StageTransition::Merge { entries };
    let store = state.store.lock().unwrap();
    match store.apply_operation(&dst_branch, op, transition) {
        Ok(new_head_op) => json_response(200, &serde_json::json!({
            "new_head_op": new_head_op,
            "dst_branch": dst_branch,
        })),
        Err(e) => write_error_response("apply merge op", e),
    }
}

/// Walk the op log from `src_head` backwards to find the latest
/// stage assigned to `sig`. Used by the commit handler to figure
/// out what stage `TakeTheirs` should land. `Ok(None)` means src
/// removed the sig.
fn resolve_take_theirs(
    state: &State,
    src_head: &Option<lex_vcs::OpId>,
    sig: &lex_vcs::SigId,
) -> std::io::Result<Option<lex_vcs::StageId>> {
    let store = state.store.lock().unwrap();
    let log = lex_vcs::OpLog::open(store.root())?;
    let Some(head) = src_head.as_ref() else { return Ok(None); };
    // Walk forward from root → head, replaying each op's transition
    // for `sig`; the last assignment wins.
    let mut current: Option<lex_vcs::StageId> = None;
    for record in log.walk_forward(head, None)? {
        match &record.produces {
            lex_vcs::StageTransition::Create { sig_id, stage_id }
                if sig_id == sig => { current = Some(stage_id.clone()); }
            lex_vcs::StageTransition::Replace { sig_id, to, .. }
                if sig_id == sig => { current = Some(to.clone()); }
            lex_vcs::StageTransition::Remove { sig_id, .. }
                if sig_id == sig => { current = None; }
            lex_vcs::StageTransition::Rename { from, to, body_stage_id }
                if from == sig || to == sig => {
                if from == sig { current = None; }
                if to == sig   { current = Some(body_stage_id.clone()); }
            }
            lex_vcs::StageTransition::Merge { entries } => {
                if let Some(opt) = entries.get(sig) {
                    current = opt.clone();
                }
            }
            _ => {}
        }
    }
    Ok(current)
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

// ---- #242: append-only sync ---------------------------------------

/// `POST /v1/ops/batch` (#242). Server endpoint for `lex op push`.
///
/// Body: a JSON array of `OperationRecord`s. The handler validates
/// DAG integrity by checking that every op's `parents` either
/// already exist on the remote *or* appear earlier in the same
/// batch. This lets a client send a topologically-ordered slice
/// without first probing for what's already there.
///
/// Response shape:
///
/// ```json
/// { "received": N, "added": M, "skipped": (N-M), "added_ids": [...] }
/// ```
///
/// Failure modes:
///
/// * `400` — body isn't a JSON array of op records.
/// * `422` with `{ "error": "MissingParent", "detail": { "op_id":
///   ..., "missing_parent": ... } }` if a parent is unreachable.
///   The whole batch is rejected; nothing is persisted. The client
///   should backfill the missing op and retry.
/// * `409` if the supplied `op_id` doesn't match the canonical
///   hash of the record's payload — content addressing must hold
///   over the wire.
///
/// Idempotency: a record whose `op_id` already exists is silently
/// skipped (not added, not rejected). Pushing the same payload
/// twice is `received == N, added == 0` on the second call.
pub(crate) fn ops_batch_handler(state: &State, body: &str)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    let records: Vec<lex_vcs::OperationRecord> = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return error_response(400,
            format!("body must be a JSON array of OperationRecord: {e}")),
    };
    let store = state.store.lock().unwrap();
    let log = match lex_vcs::OpLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening op log: {e}")),
    };

    // Validate every record before persisting any of them.
    //
    // 1. Content-addressing: the supplied `op_id` must match the
    //    canonical hash of `record.op`. Otherwise the client is
    //    sending a forged or corrupted record.
    // 2. DAG integrity: every parent must either already exist in
    //    the local log OR appear earlier in this batch.
    let mut batch_ids: std::collections::BTreeSet<lex_vcs::OpId> =
        std::collections::BTreeSet::new();
    for rec in &records {
        let expected = rec.op.op_id();
        if expected != rec.op_id {
            return error_with_detail(409, "OpIdMismatch", serde_json::json!({
                "supplied": rec.op_id,
                "expected": expected,
            }));
        }
        for parent in &rec.op.parents {
            let known = match log.get(parent) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => return error_response(500, format!("op log read: {e}")),
            };
            if !known && !batch_ids.contains(parent) {
                return error_with_detail(422, "MissingParent", serde_json::json!({
                    "op_id": rec.op_id,
                    "missing_parent": parent,
                }));
            }
        }
        batch_ids.insert(rec.op_id.clone());
    }

    // Persist. `OpLog::put` is idempotent so a re-push is a no-op
    // for already-present records.
    let mut added = 0usize;
    let mut added_ids: Vec<&lex_vcs::OpId> = Vec::new();
    for rec in &records {
        let already_present = matches!(log.get(&rec.op_id), Ok(Some(_)));
        match log.put(rec) {
            Ok(()) => {
                if !already_present {
                    added += 1;
                    added_ids.push(&rec.op_id);
                }
            }
            Err(e) => return error_response(500, format!("op log write: {e}")),
        }
    }

    json_response(200, &serde_json::json!({
        "received": records.len(),
        "added": added,
        "skipped": records.len() - added,
        "added_ids": added_ids,
    }))
}

/// `POST /v1/attestations/batch` (#242). Server endpoint for `lex
/// attest push`.
///
/// Body: a JSON array of `Attestation`s. Validates that each
/// attestation's `op_id` (when set) refers to an op that already
/// exists on the remote — `attestation_id` is then re-derivable
/// from the canonical form, so cross-store dedup just works.
///
/// Response: same shape as `ops_batch_handler` but `added_ids` is
/// the list of accepted `attestation_id`s.
///
/// Failure modes:
///
/// * `400` for malformed JSON.
/// * `422` with `{ "error": "UnknownOp", "detail": { ... } }` if
///   an attestation's `op_id` references an op the remote doesn't
///   know about. Whole batch rejected.
/// * `409` `AttestationIdMismatch` if the supplied id doesn't
///   match the canonical hash.
///
/// Idempotency: same as the ops endpoint — content-addressed dedup.
pub(crate) fn attestations_batch_handler(state: &State, body: &str)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    let attestations: Vec<lex_vcs::Attestation> = match serde_json::from_str(body) {
        Ok(a) => a,
        Err(e) => return error_response(400,
            format!("body must be a JSON array of Attestation: {e}")),
    };
    let store = state.store.lock().unwrap();
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening attestation log: {e}")),
    };
    let op_log = match lex_vcs::OpLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening op log: {e}")),
    };

    // Validate before persisting any record.
    for att in &attestations {
        // Content-addressing: re-derive attestation_id from the
        // payload and reject mismatches.
        let expected = lex_vcs::Attestation::with_timestamp(
            att.stage_id.clone(),
            att.op_id.clone(),
            att.intent_id.clone(),
            att.kind.clone(),
            att.result.clone(),
            att.produced_by.clone(),
            att.cost.clone(),
            att.timestamp,
        ).attestation_id;
        if expected != att.attestation_id {
            return error_with_detail(409, "AttestationIdMismatch", serde_json::json!({
                "supplied": att.attestation_id,
                "expected": expected,
            }));
        }
        // The op_id field, if set, must point at an op the remote
        // knows about. Without this check, attestations would
        // dangle into a future sync that never lands the op.
        if let Some(op_id) = &att.op_id {
            match op_log.get(op_id) {
                Ok(Some(_)) => {}
                Ok(None) => return error_with_detail(422, "UnknownOp", serde_json::json!({
                    "attestation_id": att.attestation_id,
                    "op_id": op_id,
                })),
                Err(e) => return error_response(500, format!("op log read: {e}")),
            }
        }
    }

    // Persist. `AttestationLog::put` is idempotent on
    // `attestation_id` and the by-stage index is rewritten as a
    // marker file, also idempotent.
    let mut added = 0usize;
    let mut added_ids: Vec<&lex_vcs::AttestationId> = Vec::new();
    for att in &attestations {
        let already_present = matches!(log.get(&att.attestation_id), Ok(Some(_)));
        match log.put(att) {
            Ok(()) => {
                if !already_present {
                    added += 1;
                    added_ids.push(&att.attestation_id);
                }
            }
            Err(e) => return error_response(500, format!("attestation log write: {e}")),
        }
    }

    json_response(200, &serde_json::json!({
        "received": attestations.len(),
        "added": added,
        "skipped": attestations.len() - added,
        "added_ids": added_ids,
    }))
}

/// `GET /v1/branches/<name>/head` (#242 follow-up). Probe endpoint
/// the `lex op push` client uses to discover the remote head before
/// computing a delta against `OpLog::ops_since`.
///
/// Response: `{ "branch": "main", "head_op": Option<OpId> }`.
/// Returns 200 even when the branch doesn't exist locally — the
/// answer in that case is `head_op: null`, which is the right
/// signal for "send everything you have."
pub(crate) fn branch_head_handler(state: &State, name: &str)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    let store = state.store.lock().unwrap();
    let head = match store.get_branch(name) {
        Ok(Some(b)) => b.head_op,
        Ok(None) => None,
        Err(e) => return error_response(500, format!("get_branch: {e}")),
    };
    json_response(200, &serde_json::json!({
        "branch": name,
        "head_op": head,
    }))
}

/// `GET /v1/ops/since?after=<op_id>&branch=<name>&limit=<n>` (#260).
/// Server endpoint for `lex op pull`.
///
/// Returns a JSON array of `OperationRecord`s reachable from
/// `branch.head_op` but not from `<after>`, sorted **oldest-first**
/// so the client can apply them in topological order without
/// re-sorting. Empty array when:
///
/// * The branch doesn't exist on the remote.
/// * The branch's `head_op` is `None`.
/// * `after == branch.head_op` (caller is already at the remote's head).
/// * `after` is *ahead of* the remote's head (caller is past the
///   remote — the symmetric "remote behind" case from #260).
///
/// `branch` defaults to `main`. `limit` caps the response — useful
/// for chunked pulls of large gaps; clients re-issue with the next
/// `after` once the prefix has landed.
///
/// Failure modes:
///
/// * `400` if the query string is malformed.
/// * `200` with `[]` for any of the empty-result cases above. "Caller
///   is already up to date" is a normal answer, not an error.
pub(crate) fn ops_since_handler(state: &State, query: &str)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    let mut after: Option<String> = None;
    let mut branch = String::from("main");
    let mut limit: Option<usize> = None;
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "after" => after = Some(v.to_string()),
            "branch" => branch = v.to_string(),
            "limit" => {
                limit = Some(match v.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => return error_response(400,
                        format!("limit must be a positive integer, got `{v}`")),
                });
            }
            _ => {}
        }
    }

    let store = state.store.lock().unwrap();
    let log = match lex_vcs::OpLog::open(store.root()) {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening op log: {e}")),
    };
    let head = match store.get_branch(&branch) {
        Ok(Some(b)) => b.head_op,
        Ok(None) => None,
        Err(e) => return error_response(500, format!("get_branch: {e}")),
    };
    let Some(head) = head else {
        return json_response(200, &serde_json::json!([]));
    };

    let ops_since = match log.ops_since(&head, after.as_ref()) {
        Ok(o) => o,
        Err(e) => return error_response(500, format!("ops_since: {e}")),
    };
    // ops_since walks newest-first; reverse so the client receives
    // oldest-first and can apply them in topological order with
    // `OpLog::put` straight through.
    let mut ops = ops_since;
    ops.reverse();
    if let Some(n) = limit {
        ops.truncate(n);
    }

    json_response(200, &serde_json::to_value(&ops).unwrap_or_default())
}

/// `GET /v1/attestations/since?after-op=<op_id>&limit=<n>` (#260).
/// Mirror of `ops_since_handler` for the attestation log.
///
/// Returns attestations whose `op_id` field is reachable from
/// **any** branch's head — not just one — and not in `after_op`'s
/// ancestry. The cross-branch fan-out matches the push side:
/// attestations are stage-keyed, not branch-keyed, so a single
/// "since this op" filter is the right shape.
///
/// Attestations with `op_id: None` (e.g. `Override`,
/// `ProducerBlock`) are always included — the cutoff doesn't apply.
/// `--limit` caps the response.
pub(crate) fn attestations_since_handler(state: &State, query: &str)
    -> Response<std::io::Cursor<Vec<u8>>>
{
    let mut after_op: Option<String> = None;
    let mut limit: Option<usize> = None;
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "after-op" => after_op = Some(v.to_string()),
            "limit" => {
                limit = Some(match v.parse::<usize>() {
                    Ok(n) => n,
                    Err(_) => return error_response(400,
                        format!("limit must be a positive integer, got `{v}`")),
                });
            }
            _ => {}
        }
    }

    let store = state.store.lock().unwrap();
    let log = match store.attestation_log() {
        Ok(l) => l,
        Err(e) => return error_response(500, format!("opening attestation log: {e}")),
    };

    // Build the exclude set: every op_id reachable from `after_op`,
    // inclusive. Attestations whose op_id is in this set were
    // already known to the caller.
    let exclude: std::collections::BTreeSet<String> = match &after_op {
        None => std::collections::BTreeSet::new(),
        Some(cutoff) => {
            let op_log = match lex_vcs::OpLog::open(store.root()) {
                Ok(l) => l,
                Err(e) => return error_response(500, format!("opening op log: {e}")),
            };
            match op_log.walk_back(cutoff, None) {
                Ok(records) => records.into_iter().map(|r| r.op_id).collect(),
                Err(_) => {
                    // Cutoff op doesn't exist on this remote. Treat
                    // as "no exclude" — caller will get every
                    // attestation. They may dedup client-side.
                    std::collections::BTreeSet::new()
                }
            }
        }
    };

    let all = match log.list_all() {
        Ok(v) => v,
        Err(e) => return error_response(500, format!("listing attestations: {e}")),
    };
    let mut filtered: Vec<lex_vcs::Attestation> = all
        .into_iter()
        .filter(|a| match &a.op_id {
            Some(op_id) => !exclude.contains(op_id),
            // No op_id = doesn't participate in the cutoff; always
            // ship it on the first pull, server-side idempotency
            // dedupes on the client.
            None => true,
        })
        .collect();
    // Stable order: oldest-first by `timestamp`, then by
    // `attestation_id` for ties. Lets the client land them
    // deterministically.
    filtered.sort_by(|a, b| {
        a.timestamp.cmp(&b.timestamp)
            .then_with(|| a.attestation_id.cmp(&b.attestation_id))
    });
    if let Some(n) = limit {
        filtered.truncate(n);
    }

    json_response(200, &serde_json::to_value(&filtered).unwrap_or_default())
}

