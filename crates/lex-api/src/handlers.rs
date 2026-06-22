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
use lex_syntax::{load_program, load_program_from_str, Manifest};
use lex_vcs::{MergeSession, MergeSessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
    /// Optional server-imposed ceiling on the effect policy honored
    /// by `/v1/run` and `/v1/replay`. `None` (the default, used by
    /// single-tenant `lex serve`) runs the caller's request policy
    /// as-is — the operator *is* the caller there, so that's
    /// intended. When `Some`, the request policy is clamped via
    /// [`clamp_policy`] so it can only *narrow* the ceiling, never
    /// widen it.
    ///
    /// Any embedder that exposes this API to untrusted callers — a
    /// hosted, multi-tenant gateway like lex-hub — MUST set this.
    /// Without it the request body can grant itself `[proc]`
    /// (arbitrary subprocess spawn), `[fs_*]` over `/`, and
    /// unrestricted `[net]`: arbitrary code execution as the server
    /// process. See lex-hub#6.
    ///
    /// NOTE: an empty scope list means "any path/host" in the
    /// runtime, so a ceiling that puts `fs_read`/`fs_write`/`net` in
    /// `allow_effects` MUST also populate the matching scope list
    /// (`allow_fs_read`, …) or it re-opens the wildcard. Granting
    /// none of those kinds is the safe default.
    pub policy_ceiling: Option<Policy>,
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
        Self::open_with_ceiling(root, None)
    }

    /// Like [`State::open`] but installs a [`policy_ceiling`](State::policy_ceiling)
    /// that `/v1/run` and `/v1/replay` clamp the caller's request
    /// policy against. Embedders exposing this API to untrusted
    /// callers must use this constructor (or set the field directly).
    pub fn open_with_ceiling(
        root: PathBuf,
        policy_ceiling: Option<Policy>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            store: Mutex::new(Store::open(&root)?),
            root,
            sessions: Mutex::new(HashMap::new()),
            policy_ceiling,
        })
    }

    /// Construct a per-tenant `State` by prefixing `store_root` with the
    /// tenant id. Single-tenant `lex serve` is unaffected — it calls
    /// `State::open` directly.
    ///
    /// `tenant_id` is restricted to `[A-Za-z0-9_-]{1,64}`: anything else
    /// (path separators, `..`, NUL, absolute paths, dotfiles, empty
    /// string) is rejected before touching the filesystem. Without this
    /// `PathBuf::join("/etc")` would silently replace `store_root`, and
    /// `PathBuf::join("../foo")` would escape the tenant root.
    pub fn new_with_tenant(tenant_id: &str, store_root: PathBuf) -> anyhow::Result<Self> {
        validate_tenant_id(tenant_id)?;
        Self::open(store_root.join(tenant_id))
    }

    /// Multi-tenant constructor that also installs a policy ceiling
    /// for `/v1/run` / `/v1/replay`. The path-traversal guard from
    /// [`new_with_tenant`](State::new_with_tenant) and the effect
    /// ceiling are the two halves a hosted gateway needs.
    pub fn new_with_tenant_and_ceiling(
        tenant_id: &str,
        store_root: PathBuf,
        policy_ceiling: Option<Policy>,
    ) -> anyhow::Result<Self> {
        validate_tenant_id(tenant_id)?;
        Self::open_with_ceiling(store_root.join(tenant_id), policy_ceiling)
    }
}

/// Clamp a caller-supplied [`Policy`] to a server-imposed `ceiling`
/// so it can only *narrow* the granted capabilities, never widen
/// them. Used by [`run_handler`] when [`State::policy_ceiling`] is
/// set — i.e. when an embedder exposes `/v1/run` to untrusted
/// callers and must not let the request body grant itself `[proc]`,
/// arbitrary `[fs_*]` paths, or unrestricted `[net]`.
///
/// - **Effects**: set-intersection of request and ceiling. The
///   caller may drop effects but never add one the ceiling withheld.
/// - **Scopes** (fs paths, proc binaries, net hosts): taken from the
///   ceiling outright. The caller cannot widen them, and — because an
///   empty scope list means "any" in the runtime — we must not let a
///   caller's empty list collapse the ceiling's restriction back to a
///   wildcard.
/// - **Budget**: the more restrictive (smaller) of the two.
fn clamp_policy(requested: Policy, ceiling: &Policy) -> Policy {
    let allow_effects: BTreeSet<String> = requested
        .allow_effects
        .intersection(&ceiling.allow_effects)
        .cloned()
        .collect();
    let budget = match (requested.budget, ceiling.budget) {
        (Some(r), Some(c)) => Some(r.min(c)),
        (None, Some(c)) => Some(c),
        (Some(r), None) => Some(r),
        (None, None) => None,
    };
    Policy {
        allow_effects,
        allow_fs_read: ceiling.allow_fs_read.clone(),
        allow_fs_write: ceiling.allow_fs_write.clone(),
        allow_net_host: ceiling.allow_net_host.clone(),
        allow_proc: ceiling.allow_proc.clone(),
        budget,
    }
}

fn validate_tenant_id(tenant_id: &str) -> anyhow::Result<()> {
    if tenant_id.is_empty() {
        anyhow::bail!("tenant_id must not be empty");
    }
    if tenant_id.len() > 64 {
        anyhow::bail!("tenant_id must be at most 64 bytes");
    }
    if !tenant_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!(
            "tenant_id {tenant_id:?} contains characters outside [A-Za-z0-9_-]"
        );
    }
    Ok(())
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
    // #292 slice 3: budget overflow → 503 with `Retry-After: 0`.
    // Unlike Contention (where a retry might land after another
    // writer finishes), there's no point retrying a budget-
    // exceeded op — the caller needs to raise the cap, switch
    // sessions, or refactor the work. The `Retry-After: 0`
    // signals "don't bother retrying as-is" while still using
    // the canonical "service refused" status code.
    if let lex_store::StoreError::BudgetExceeded { session_id, cap, spent_after } = &err {
        let body = serde_json::to_vec(&ErrorEnvelope {
            error: format!(
                "{prefix}: session `{session_id}` budget exceeded \
                 (spent_after={spent_after}, cap={cap})"
            ),
            detail: Some(serde_json::json!({
                "kind": "budget_exceeded",
                "session_id": session_id,
                "cap": cap,
                "spent_after": spent_after,
            })),
        }).unwrap_or_else(|_| b"{}".to_vec());
        return Response::from_data(body)
            .with_status_code(503)
            .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
            .with_header(Header::from_bytes(&b"Retry-After"[..], &b"0"[..]).unwrap());
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

    // POST /v1/pkg/publish sends a raw tar.gz body — read bytes before routing.
    if matches!(method, Method::Post) && path == "/v1/pkg/publish" {
        let mut body_bytes: Vec<u8> = Vec::new();
        let _ = req.as_reader().read_to_end(&mut body_bytes);
        let resp = pkg_publish_handler(&state, &body_bytes);
        return req.respond(resp);
    }

    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);

    let resp = route(&state, &method, &path, &query, &body, x_lex_user.as_deref());
    req.respond(resp)
}

/// Auth-gated entry point. Calls `auth(path, headers)` before routing;
/// returns 401 JSON when it returns false. Keeps auth logic out of the
/// product-agnostic core.
pub fn handle_with_auth<F>(state: Arc<State>, req: Request, auth: F) -> std::io::Result<()>
where
    F: FnOnce(&str, &[Header]) -> bool,
{
    let path = req.url().split('?').next().unwrap_or("").to_string();
    if !auth(&path, req.headers()) {
        return req.respond(
            Response::from_data(br#"{"error":"unauthorized"}"#.to_vec())
                .with_status_code(401)
                .with_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
                ),
        );
    }
    handle(state, req)
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
        // ---- #4: package concept ----------------------------------
        // POST /v1/pkg/publish is handled in handle() before route()
        // (binary body), so it doesn't appear here.
        (Method::Get, "/v1/pkg") => pkg_list_handler(state),
        // Owner-only visibility toggle (authed via the front door). Must
        // precede the generic `/v1/pkg/{name}` arms; it's a PUT, so it
        // can't collide with the GET/DELETE arms regardless.
        (Method::Put, p) if p.starts_with("/v1/pkg/") && p.ends_with("/visibility") => {
            let name = &p["/v1/pkg/".len()..p.len() - "/visibility".len()];
            pkg_set_visibility_handler(state, name, body)
        }
        (Method::Get, p) if p.starts_with("/v1/pkg/") && p.ends_with("/head") => {
            let name = &p["/v1/pkg/".len()..p.len() - "/head".len()];
            pkg_head_handler(state, name)
        }
        (Method::Get, p) if p.starts_with("/v1/pkg/") && p.ends_with("/versions") => {
            let name = &p["/v1/pkg/".len()..p.len() - "/versions".len()];
            pkg_versions_handler(state, name)
        }
        // /v1/pkg/{name}/{version}/archive — must match before the generic /{name}/{version}
        (Method::Get, p) if p.starts_with("/v1/pkg/") && p.ends_with("/archive") => {
            let inner = &p["/v1/pkg/".len()..p.len() - "/archive".len()];
            // inner = "{name}/{version}"
            if let Some((name, version)) = inner.split_once('/') {
                pkg_archive_handler(state, name, version)
            } else {
                error_response(400, "expected /v1/pkg/{name}/{version}/archive")
            }
        }
        // /v1/pkg/{name}/{version}
        (Method::Get, p) if p.starts_with("/v1/pkg/") && p["/v1/pkg/".len()..].contains('/') => {
            let inner = &p["/v1/pkg/".len()..];
            if let Some((name, version)) = inner.split_once('/') {
                pkg_get_version_handler(state, name, version)
            } else {
                error_response(400, "expected /v1/pkg/{name}/{version}")
            }
        }
        (Method::Get, p) if p.starts_with("/v1/pkg/") => {
            let name = &p["/v1/pkg/".len()..];
            pkg_get_handler(state, name)
        }
        (Method::Delete, p) if p.starts_with("/v1/pkg/") => {
            let name = &p["/v1/pkg/".len()..];
            pkg_delete_handler(state, name)
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
    let mut policy = req.policy.into_policy();
    // When a server-imposed ceiling is present (multi-tenant
    // embedders like lex-hub), the request policy can only narrow
    // it — never grant itself proc/fs/net beyond what the operator
    // allowed. Single-tenant `lex serve` leaves the ceiling unset
    // and runs the caller's policy verbatim.
    if let Some(ceiling) = &state.policy_ceiling {
        policy = clamp_policy(policy, ceiling);
    }
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

// ── Package concept (#4) ────────────────────────────────────────────────────

/// Per-version record stored at `{store_root}/packages/{name}/{version}.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PkgRecord {
    name: String,
    version: String,
    head_op: Option<String>,
    published_at: u64,
    /// Function names introduced or updated by this version (for retract).
    function_names: Vec<String>,
    /// Raw op JSON from each file in this publish.
    ops: Vec<serde_json::Value>,
}

/// Whether a package is reachable without authentication.
///
/// Per-package (applies across all versions), GitHub-style: an org's
/// store can hold a mix of public and private packages. Defaults to
/// `Private`, so an index written before this field existed (or any
/// freshly published package) is private until an owner opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Private,
    Public,
}

/// Index stored at `{store_root}/packages/{name}/index.json`.
///
/// Tracks which versions have been published and which is "latest", so
/// consumers can resolve `{name}@latest` without listing every file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct PkgIndex {
    /// The most recently published version string (human label, not OpId).
    latest: Option<String>,
    /// All published versions, newest-last.
    versions: Vec<PkgVersionSummary>,
    /// Public/private flag. `#[serde(default)]` keeps pre-existing
    /// `index.json` files (which lack the field) deserializing as
    /// `Private`.
    #[serde(default)]
    visibility: Visibility,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PkgVersionSummary {
    version: String,
    head_op: Option<String>,
    published_at: u64,
}

fn pkg_name_dir(root: &std::path::Path, name: &str) -> PathBuf {
    root.join("packages").join(name)
}

fn pkg_index_path(root: &std::path::Path, name: &str) -> PathBuf {
    pkg_name_dir(root, name).join("index.json")
}

fn pkg_version_path(root: &std::path::Path, name: &str, version: &str) -> PathBuf {
    pkg_name_dir(root, name).join(format!("{version}.json"))
}

fn pkg_archive_path(root: &std::path::Path, name: &str, version: &str) -> PathBuf {
    pkg_name_dir(root, name).join(format!("{version}.tar.gz"))
}

fn load_pkg_index(root: &std::path::Path, name: &str) -> Option<PkgIndex> {
    let bytes = std::fs::read(pkg_index_path(root, name)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn load_pkg_record(root: &std::path::Path, name: &str, version: &str) -> Option<PkgRecord> {
    let bytes = std::fs::read(pkg_version_path(root, name, version)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn load_latest_pkg_record(root: &std::path::Path, name: &str) -> Option<PkgRecord> {
    let index = load_pkg_index(root, name)?;
    let latest = index.latest.clone()?;
    load_pkg_record(root, name, &latest)
}

/// A package is public iff its index exists and is marked `Public`.
/// A missing index (unknown package) is treated as private, so the
/// public surface never distinguishes "private" from "does not exist".
fn pkg_is_public(root: &std::path::Path, name: &str) -> bool {
    load_pkg_index(root, name).map(|i| i.visibility) == Some(Visibility::Public)
}

/// Reject package/version path segments that could escape the
/// `packages/` directory or otherwise aren't valid names. Mirrors the
/// tenant-id guard's spirit (defense in depth — lex-hub validates the
/// tenant, this validates the package/version).
fn valid_pkg_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[derive(Deserialize)]
struct VisibilityReq {
    visibility: Visibility,
}

/// `PUT /v1/pkg/{name}/visibility` — set a package public or private.
///
/// Authorization is implicit: this runs against a single tenant's store,
/// and the caller only reaches *this* store because the front door
/// (lex-hub) authenticated their token and selected it. A caller can
/// therefore only change visibility of packages they own.
fn pkg_set_visibility_handler(
    state: &State,
    name: &str,
    body: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    if !valid_pkg_segment(name) {
        return error_response(400, format!("invalid package name {name:?}"));
    }
    let req: VisibilityReq = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let mut index = match load_pkg_index(&state.root, name) {
        Some(i) => i,
        None => return error_response(404, format!("package {name:?} not found")),
    };
    index.visibility = req.visibility;
    let bytes = serde_json::to_vec_pretty(&index).unwrap_or_default();
    match std::fs::write(pkg_index_path(&state.root, name), bytes) {
        Ok(()) => json_response(
            200,
            &serde_json::json!({ "name": name, "visibility": index.visibility }),
        ),
        Err(e) => error_response(500, format!("write index: {e}")),
    }
}

/// Names of the tenant's PUBLIC packages, sorted. Pure (filesystem in,
/// names out) so the visibility filter is unit-testable.
fn public_pkg_names(root: &std::path::Path) -> Vec<String> {
    list_pkg_names(root)
        .into_iter()
        .filter(|name| pkg_is_public(root, name))
        .collect()
}

/// `GET /v1/public/<tenant>` (org page) — list only the tenant's PUBLIC
/// packages (latest version of each). Private packages are omitted, so
/// their existence is not revealed.
fn public_pkg_list_handler(state: &State) -> Response<std::io::Cursor<Vec<u8>>> {
    let packages: Vec<serde_json::Value> = public_pkg_names(&state.root)
        .iter()
        .filter_map(|name| {
            let r = load_latest_pkg_record(&state.root, name)?;
            Some(serde_json::json!({
                "name": r.name,
                "version": r.version,
                "head_op": r.head_op,
                "published_at": r.published_at,
            }))
        })
        .collect();
    json_response(200, &serde_json::json!({ "packages": packages }))
}

/// A resolved public read target. Separated from response formatting so
/// the routing/guard logic is unit-testable without constructing HTTP
/// responses. `Err(status)` is a guard failure (405 = non-GET,
/// 404 = invalid/unknown route).
#[derive(Debug, PartialEq, Eq)]
enum PublicTarget {
    List,
    Latest(String),
    Versions(String),
    Head(String),
    Version(String, String),
    Archive(String, String),
}

impl PublicTarget {
    /// The package name a target refers to, if any (`List` has none).
    fn pkg_name(&self) -> Option<&str> {
        match self {
            PublicTarget::List => None,
            PublicTarget::Latest(n)
            | PublicTarget::Versions(n)
            | PublicTarget::Head(n)
            | PublicTarget::Version(n, _)
            | PublicTarget::Archive(n, _) => Some(n),
        }
    }
}

/// Pure routing decision for `/v1/public/<tenant>` reads. `path` is the
/// portion after the tenant (leading `/` ok). Enforces GET-only and
/// per-segment name validation; does NOT consult the store (visibility
/// is checked by the caller, which has store access).
fn resolve_public(method: &Method, path: &str) -> Result<PublicTarget, u16> {
    if !matches!(method, Method::Get) {
        return Err(405);
    }
    let rest = path.trim_matches('/');
    if rest.is_empty() {
        return Ok(PublicTarget::List);
    }
    let segs: Vec<&str> = rest.split('/').collect();
    if !segs.iter().all(|s| valid_pkg_segment(s)) {
        return Err(404);
    }
    match segs.as_slice() {
        [n] => Ok(PublicTarget::Latest(n.to_string())),
        [n, "versions"] => Ok(PublicTarget::Versions(n.to_string())),
        [n, "head"] => Ok(PublicTarget::Head(n.to_string())),
        [n, v, "archive"] => Ok(PublicTarget::Archive(n.to_string(), v.to_string())),
        [n, v] => Ok(PublicTarget::Version(n.to_string(), v.to_string())),
        _ => Err(404),
    }
}

/// Unauthenticated, read-only access to **public** packages in `state`'s
/// store. `path` is the portion of the URL after `/v1/public/<tenant>`
/// (with a leading `/`); lex-hub resolves `<tenant>` → store and calls
/// this. Visibility gating, GET-only enforcement, and segment validation
/// all live here so the whole public surface is auditable in one place.
///
/// Everything served here is package-scoped — manifests and the source
/// archive of a public package's own publish — so it cannot leak code
/// from a private package that happens to share content-addressed stages
/// in the same store.
pub fn route_public(
    state: &State,
    method: &Method,
    path: &str,
    _query: &str,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let target = match resolve_public(method, path) {
        Ok(t) => t,
        Err(405) => return error_response(405, "public read is GET-only"),
        Err(_) => return error_response(404, "not found"),
    };
    // The org listing already filters to public packages itself.
    if let PublicTarget::List = target {
        return public_pkg_list_handler(state);
    }
    // Single 404 for both "private" and "absent" — never reveal which.
    if let Some(name) = target.pkg_name() {
        if !pkg_is_public(&state.root, name) {
            return error_response(404, format!("package {name:?} not found"));
        }
    }
    match target {
        PublicTarget::List => unreachable!("handled above"),
        PublicTarget::Latest(n) => pkg_get_handler(state, &n),
        PublicTarget::Versions(n) => pkg_versions_handler(state, &n),
        PublicTarget::Head(n) => pkg_head_handler(state, &n),
        PublicTarget::Version(n, v) => pkg_get_version_handler(state, &n, &v),
        PublicTarget::Archive(n, v) => pkg_archive_handler(state, &n, &v),
    }
}

fn save_pkg_record(
    root: &std::path::Path,
    record: &PkgRecord,
    archive: &[u8],
) -> std::io::Result<()> {
    let dir = pkg_name_dir(root, &record.name);
    std::fs::create_dir_all(&dir)?;

    // Per-version record.
    let rec_bytes = serde_json::to_vec_pretty(record).unwrap_or_default();
    std::fs::write(pkg_version_path(root, &record.name, &record.version), rec_bytes)?;

    // Archive (tar.gz) for the download endpoint.
    std::fs::write(pkg_archive_path(root, &record.name, &record.version), archive)?;

    // Update the index.
    let mut index = load_pkg_index(root, &record.name).unwrap_or_default();
    index.latest = Some(record.version.clone());
    if !index.versions.iter().any(|v| v.version == record.version) {
        index.versions.push(PkgVersionSummary {
            version: record.version.clone(),
            head_op: record.head_op.clone(),
            published_at: record.published_at,
        });
    }
    let idx_bytes = serde_json::to_vec_pretty(&index).unwrap_or_default();
    std::fs::write(pkg_index_path(root, &record.name), idx_bytes)
}

fn list_pkg_names(root: &std::path::Path) -> Vec<String> {
    let dir = root.join("packages");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn collect_lex_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_lex_files(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("lex") {
            out.push(path);
        }
    }
}

/// `POST /v1/pkg/publish` — publish a multi-file package from a `.tar.gz`
/// archive containing `lex.toml` and `src/**/*.lex`.
fn pkg_publish_handler(state: &State, body: &[u8]) -> Response<std::io::Cursor<Vec<u8>>> {
    let tmp = match tempfile::TempDir::new() {
        Ok(t) => t,
        Err(e) => return error_response(500, format!("create temp dir: {e}")),
    };
    {
        let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(body));
        let mut ar = tar::Archive::new(gz);
        if let Err(e) = ar.unpack(tmp.path()) {
            return error_response(400, format!("unpack archive: {e}"));
        }
    }

    let toml_path = tmp.path().join("lex.toml");
    if !toml_path.exists() {
        return error_response(400, "archive must contain lex.toml at root");
    }
    let manifest = match Manifest::load(&toml_path) {
        Ok(m) => m,
        Err(e) => return error_response(400, format!("lex.toml: {e}")),
    };
    let (pkg_name, pkg_version) = match &manifest.package {
        Some(m) => (m.name.clone(), m.version.clone()),
        None => return error_response(400, "lex.toml must have a [package] section"),
    };

    let src_dir = tmp.path().join("src");
    if !src_dir.exists() {
        return error_response(400, "archive must contain a src/ directory");
    }
    let mut lex_files: Vec<PathBuf> = Vec::new();
    collect_lex_files(&src_dir, &mut lex_files);
    if lex_files.is_empty() {
        return error_response(400, "no .lex files found in src/");
    }

    let store = state.store.lock().unwrap();
    let branch = store.current_branch();

    let mut all_ops: Vec<serde_json::Value> = Vec::new();
    let mut final_head_op: Option<String> = None;
    let mut all_function_names: Vec<String> = Vec::new();

    for lex_path in &lex_files {
        let prog = match load_program(lex_path) {
            Ok(p) => p,
            Err(e) => return error_response(400, format!("load {}: {e}", lex_path.display())),
        };
        let mut stages = canonicalize_program(&prog);
        if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
            return error_with_detail(
                422,
                format!("type errors in {}", lex_path.display()),
                serde_json::to_value(&errs).unwrap(),
            );
        }

        let old_head = match store.branch_head(&branch) {
            Ok(h) => h,
            Err(e) => return error_response(500, format!("branch_head: {e}")),
        };
        let old_fns: BTreeMap<String, lex_ast::FnDecl> = old_head.values()
            .filter_map(|stg| store.get_ast(stg).ok())
            .filter_map(|s| match s {
                lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd)),
                _ => None,
            })
            .collect();
        let new_fns: BTreeMap<String, lex_ast::FnDecl> = stages.iter()
            .filter_map(|s| match s {
                lex_ast::Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())),
                _ => None,
            })
            .collect();

        for name in new_fns.keys() {
            if !all_function_names.contains(name) {
                all_function_names.push(name.clone());
            }
        }

        let report = lex_vcs::compute_diff(&old_fns, &new_fns, false);

        let file_key = lex_path
            .strip_prefix(tmp.path())
            .unwrap_or(lex_path)
            .display()
            .to_string();
        let mut new_imports = lex_vcs::ImportMap::new();
        {
            let entry = new_imports.entry(file_key).or_default();
            for s in &stages {
                if let lex_ast::Stage::Import(im) = s {
                    entry.insert(im.reference.clone());
                }
            }
        }

        match store.publish_program(&branch, &stages, &report, &new_imports, false) {
            Ok(outcome) => {
                let ops_json = serde_json::to_value(&outcome.ops).unwrap_or_default();
                if let serde_json::Value::Array(arr) = ops_json {
                    all_ops.extend(arr);
                }
                if let Some(h) = outcome.head_op {
                    final_head_op = Some(h);
                }
            }
            Err(lex_store::StoreError::TypeError(errs)) => {
                return error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap());
            }
            Err(e) => return write_error_response("publish_program", e),
        }
    }

    // Reject re-publish of the same (name, version) to keep the op log stable.
    if load_pkg_record(&state.root, &pkg_name, &pkg_version).is_some() {
        return error_response(
            409,
            format!(
                "package {pkg_name}@{pkg_version} already published; \
                 bump the version in lex.toml to publish a new release"
            ),
        );
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = PkgRecord {
        name: pkg_name.clone(),
        version: pkg_version,
        head_op: final_head_op.clone(),
        published_at: now,
        function_names: all_function_names,
        ops: all_ops.clone(),
    };
    if let Err(e) = save_pkg_record(&state.root, &record, body) {
        return error_response(500, format!("save package index: {e}"));
    }

    json_response(200, &serde_json::json!({
        "package": pkg_name,
        "ops": all_ops,
        "head_op": final_head_op,
    }))
}

/// `GET /v1/pkg` — list packages published by this tenant (latest version of each).
fn pkg_list_handler(state: &State) -> Response<std::io::Cursor<Vec<u8>>> {
    let names = list_pkg_names(&state.root);
    let packages: Vec<serde_json::Value> = names.iter()
        .filter_map(|name| {
            let idx = load_pkg_index(&state.root, name)?;
            let latest = idx.latest.as_deref()?;
            let r = load_pkg_record(&state.root, name, latest)?;
            Some(serde_json::json!({
                "name": r.name,
                "version": r.version,
                "head_op": r.head_op,
                "published_at": r.published_at,
            }))
        })
        .collect();
    json_response(200, &serde_json::json!({ "packages": packages }))
}

/// `GET /v1/pkg/{name}` — latest version details for a package.
fn pkg_get_handler(state: &State, name: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match load_latest_pkg_record(&state.root, name) {
        Some(r) => json_response(200, &serde_json::json!({
            "name": r.name,
            "version": r.version,
            "head_op": r.head_op,
            "published_at": r.published_at,
            "function_names": r.function_names,
            "ops": r.ops,
        })),
        None => error_response(404, format!("package {name:?} not found")),
    }
}

/// `GET /v1/pkg/{name}/versions` — all published versions for a package.
fn pkg_versions_handler(state: &State, name: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match load_pkg_index(&state.root, name) {
        Some(idx) => json_response(200, &serde_json::json!({
            "name": name,
            "latest": idx.latest,
            "versions": idx.versions,
        })),
        None => error_response(404, format!("package {name:?} not found")),
    }
}

/// `GET /v1/pkg/{name}/{version}` — specific version details.
fn pkg_get_version_handler(state: &State, name: &str, version: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match load_pkg_record(&state.root, name, version) {
        Some(r) => json_response(200, &serde_json::json!({
            "name": r.name,
            "version": r.version,
            "head_op": r.head_op,
            "published_at": r.published_at,
            "function_names": r.function_names,
            "ops": r.ops,
        })),
        None => error_response(404, format!("package {name:?}@{version:?} not found")),
    }
}

/// `GET /v1/pkg/{name}/{version}/archive` — download the source tar.gz.
fn pkg_archive_handler(state: &State, name: &str, version: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let path = pkg_archive_path(&state.root, name, version);
    match std::fs::read(&path) {
        Ok(bytes) => Response::from_data(bytes)
            .with_status_code(200)
            .with_header(
                tiny_http::Header::from_bytes(
                    &b"Content-Type"[..],
                    &b"application/gzip"[..],
                )
                .unwrap(),
            ),
        Err(_) => error_response(404, format!("archive for {name:?}@{version:?} not found")),
    }
}

/// `GET /v1/pkg/{name}/head` — head op for a package's latest version.
fn pkg_head_handler(state: &State, name: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    match load_latest_pkg_record(&state.root, name) {
        Some(r) => json_response(200, &serde_json::json!({
            "name": r.name,
            "version": r.version,
            "head_op": r.head_op,
        })),
        None => error_response(404, format!("package {name:?} not found")),
    }
}

/// `DELETE /v1/pkg/{name}` — retract the latest version of a package.
fn pkg_delete_handler(state: &State, name: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let record = match load_latest_pkg_record(&state.root, name) {
        Some(r) => r,
        None => return error_response(404, format!("package {name:?} not found")),
    };

    let store = state.store.lock().unwrap();
    let branch = store.current_branch();

    let head = match store.branch_head(&branch) {
        Ok(h) => h,
        Err(e) => return error_response(500, format!("branch_head: {e}")),
    };

    // Build old_fns from this package's function names that are still on the branch.
    let old_fns: BTreeMap<String, lex_ast::FnDecl> = head.values()
        .filter_map(|stage_id| store.get_ast(stage_id).ok())
        .filter_map(|s| match s {
            lex_ast::Stage::FnDecl(fd)
                if record.function_names.contains(&fd.name) => Some((fd.name.clone(), fd)),
            _ => None,
        })
        .collect();

    let new_fns: BTreeMap<String, lex_ast::FnDecl> = BTreeMap::new();
    let report = lex_vcs::compute_diff(&old_fns, &new_fns, false);
    let empty_imports = lex_vcs::ImportMap::new();

    match store.publish_program(&branch, &[], &report, &empty_imports, false) {
        Ok(outcome) => {
            // Remove the version record and archive, then update the index.
            let ver = record.version.clone();
            let _ = std::fs::remove_file(pkg_version_path(&state.root, name, &ver));
            let _ = std::fs::remove_file(pkg_archive_path(&state.root, name, &ver));
            // Update index: remove this version, set latest to previous if any.
            if let Some(mut idx) = load_pkg_index(&state.root, name) {
                idx.versions.retain(|v| v.version != ver);
                idx.latest = idx.versions.last().map(|v| v.version.clone());
                if idx.versions.is_empty() {
                    let _ = std::fs::remove_dir_all(pkg_name_dir(&state.root, name));
                } else {
                    let bytes = serde_json::to_vec_pretty(&idx).unwrap_or_default();
                    let _ = std::fs::write(pkg_index_path(&state.root, name), bytes);
                }
            }
            json_response(200, &serde_json::json!({
                "deleted": name,
                "version": ver,
                "ops": outcome.ops,
                "head_op": outcome.head_op,
            }))
        }
        Err(lex_store::StoreError::TypeError(errs)) => {
            error_with_detail(422, "type errors", serde_json::to_value(&errs).unwrap())
        }
        Err(e) => write_error_response("retract package", e),
    }
}

#[cfg(test)]
mod policy_ceiling_tests {
    use super::*;
    use lex_runtime::Policy;
    use std::path::PathBuf;

    /// A maximally-permissive policy of the kind a malicious caller
    /// would put in a `/v1/run` body: every dangerous effect plus
    /// fs over `/`.
    fn permissive_request() -> Policy {
        Policy {
            allow_effects: ["io", "fs_read", "fs_write", "net", "proc"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allow_fs_read: vec![PathBuf::from("/")],
            allow_fs_write: vec![PathBuf::from("/")],
            allow_net_host: Vec::new(),
            allow_proc: Vec::new(),
            budget: None,
        }
    }

    #[test]
    fn ceiling_drops_effects_the_caller_was_not_granted() {
        let ceiling = Policy {
            allow_effects: ["io", "time"].iter().map(|s| s.to_string()).collect(),
            ..Policy::default()
        };
        let got = clamp_policy(permissive_request(), &ceiling);
        assert!(got.allow_effects.contains("io"));
        assert!(!got.allow_effects.contains("proc"), "proc must not survive a ceiling without it");
        assert!(!got.allow_effects.contains("fs_write"));
        assert!(!got.allow_effects.contains("net"));
        // `time` is in the ceiling but not the request → intersection drops it.
        assert!(!got.allow_effects.contains("time"));
    }

    #[test]
    fn ceiling_scopes_override_caller_scopes() {
        let ceiling = Policy {
            allow_effects: ["fs_read"].iter().map(|s| s.to_string()).collect(),
            allow_fs_read: vec![PathBuf::from("/srv/tenant")],
            ..Policy::default()
        };
        let got = clamp_policy(permissive_request(), &ceiling);
        // Caller asked for "/" but only the ceiling's scope survives —
        // an empty/wider caller list can never widen the ceiling.
        assert_eq!(got.allow_fs_read, vec![PathBuf::from("/srv/tenant")]);
        assert!(got.allow_fs_write.is_empty());
        assert!(got.allow_proc.is_empty());
        assert!(got.allow_net_host.is_empty());
    }

    #[test]
    fn ceiling_caps_budget_and_prefers_the_smaller() {
        // Caller wants unlimited; ceiling caps it.
        let mut req = permissive_request();
        req.budget = None;
        let ceiling = Policy { budget: Some(1_000), ..Policy::default() };
        assert_eq!(clamp_policy(req, &ceiling).budget, Some(1_000));

        // Caller asks for less than the ceiling → keep the caller's.
        let mut req2 = permissive_request();
        req2.budget = Some(50);
        let ceiling2 = Policy { budget: Some(1_000), ..Policy::default() };
        assert_eq!(clamp_policy(req2, &ceiling2).budget, Some(50));
    }

    #[test]
    fn empty_ceiling_is_pure_only() {
        let got = clamp_policy(permissive_request(), &Policy::default());
        assert!(got.allow_effects.is_empty(), "an empty ceiling grants nothing");
        assert!(got.allow_proc.is_empty());
        assert!(got.allow_fs_write.is_empty());
    }
}

#[cfg(test)]
mod public_read_tests {
    use super::*;

    /// Write a minimal package (index + per-version record + an archive
    /// blob) straight into a temp store, bypassing the publish pipeline.
    fn seed_pkg(root: &std::path::Path, name: &str, version: &str) {
        let record = PkgRecord {
            name: name.to_string(),
            version: version.to_string(),
            head_op: Some(format!("op-{name}")),
            published_at: 1,
            function_names: vec![format!("{name}.f")],
            ops: vec![],
        };
        save_pkg_record(root, &record, format!("ARCHIVE:{name}@{version}").as_bytes())
            .expect("seed package");
    }

    #[test]
    fn new_package_defaults_to_private() {
        let tmp = tempfile::TempDir::new().unwrap();
        seed_pkg(tmp.path(), "lex-schema", "0.9.2");
        assert!(!pkg_is_public(tmp.path(), "lex-schema"));
        // Unknown packages are also "not public" — never distinguished.
        assert!(!pkg_is_public(tmp.path(), "does-not-exist"));
    }

    #[test]
    fn set_visibility_round_trips_and_index_persists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = State::open(tmp.path().to_path_buf()).unwrap();
        seed_pkg(tmp.path(), "lex-schema", "0.9.2");

        let _ = pkg_set_visibility_handler(&state, "lex-schema", r#"{"visibility":"public"}"#);
        assert!(pkg_is_public(tmp.path(), "lex-schema"));
        // The version list survives the index rewrite (we don't clobber it).
        let idx = load_pkg_index(tmp.path(), "lex-schema").unwrap();
        assert_eq!(idx.latest.as_deref(), Some("0.9.2"));
        assert_eq!(idx.versions.len(), 1);

        let _ = pkg_set_visibility_handler(&state, "lex-schema", r#"{"visibility":"private"}"#);
        assert!(!pkg_is_public(tmp.path(), "lex-schema"));
    }

    #[test]
    fn set_visibility_on_unknown_package_is_a_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = State::open(tmp.path().to_path_buf()).unwrap();
        // No package seeded → handler returns 404 and writes nothing.
        let _ = pkg_set_visibility_handler(&state, "ghost", r#"{"visibility":"public"}"#);
        assert!(load_pkg_index(tmp.path(), "ghost").is_none());
    }

    #[test]
    fn public_listing_omits_private_packages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = State::open(tmp.path().to_path_buf()).unwrap();
        seed_pkg(tmp.path(), "pub-pkg", "1.0.0");
        seed_pkg(tmp.path(), "priv-pkg", "1.0.0");
        let _ = pkg_set_visibility_handler(&state, "pub-pkg", r#"{"visibility":"public"}"#);

        let names = public_pkg_names(tmp.path());
        assert_eq!(names, vec!["pub-pkg".to_string()]);
    }

    #[test]
    fn resolve_public_maps_routes() {
        let get = Method::Get;
        assert_eq!(resolve_public(&get, "").unwrap(), PublicTarget::List);
        assert_eq!(resolve_public(&get, "/").unwrap(), PublicTarget::List);
        assert_eq!(
            resolve_public(&get, "/lex-schema").unwrap(),
            PublicTarget::Latest("lex-schema".into())
        );
        assert_eq!(
            resolve_public(&get, "/lex-schema/versions").unwrap(),
            PublicTarget::Versions("lex-schema".into())
        );
        assert_eq!(
            resolve_public(&get, "/lex-schema/head").unwrap(),
            PublicTarget::Head("lex-schema".into())
        );
        assert_eq!(
            resolve_public(&get, "/lex-schema/0.9.2").unwrap(),
            PublicTarget::Version("lex-schema".into(), "0.9.2".into())
        );
        assert_eq!(
            resolve_public(&get, "/lex-schema/0.9.2/archive").unwrap(),
            PublicTarget::Archive("lex-schema".into(), "0.9.2".into())
        );
    }

    #[test]
    fn resolve_public_rejects_bad_method_and_traversal() {
        // Non-GET → 405.
        assert_eq!(resolve_public(&Method::Put, "/lex-schema"), Err(405));
        assert_eq!(resolve_public(&Method::Post, "").err(), Some(405));
        // Path traversal / invalid segments → 404, never a filesystem touch.
        assert_eq!(resolve_public(&Method::Get, "/.."), Err(404));
        assert_eq!(resolve_public(&Method::Get, "/lex-schema/../etc"), Err(404));
        assert_eq!(resolve_public(&Method::Get, "/a/b/c/d"), Err(404));
        // Slashes elsewhere can't smuggle a deep path: each segment is checked.
        assert!(resolve_public(&Method::Get, "/lex schema").is_err());
    }
}
