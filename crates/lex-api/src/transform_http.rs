//! The typed-transform write surface (#837 piece A).
//!
//! `POST /v1/transform` lets an agent harness make a *typed* edit — one of
//! #280's four transforms — straight through the op log, instead of editing
//! text and running `lex publish`. The same code path backs `lex ws transform`
//! (an embedded on-disk store, no server): [`apply_transform`] is the single
//! function both call, so the two cannot diverge.
//!
//! # Request
//!
//! ```json
//! {
//!   "branch": "main",                       // REQUIRED. Never the server's global current branch.
//!   "intent": {                             // optional; absent => explicitly unattributed (#970)
//!     "prompt": "why", "model": "provider/name", "session": "s-1", "issue_id": "..."
//!   },
//!   "transform": { "kind": "...", ... }     // kind-specific, below
//! }
//! ```
//!
//! | `kind` | params | ops emitted |
//! |---|---|---|
//! | `replace_match_arm` | `from_stage_id`, `match_node`, `arm_index`, `new_body` (CExpr) | `ReplaceMatchArm` |
//! | `rename_local` | `from_stage_id`, `let_node`, `new_name` | `RenameLocal` |
//! | `inline_let` | `from_stage_id`, `let_node` | `InlineLet` |
//! | `extract_function` | `from_stage_id`, `expr_node`, `spec {name, type_params?, params, return_type, effects?}` | `AddFunction` + `ModifyBody` |
//!
//! `from_stage_id` must be the stage the branch head currently binds to the
//! function's signature (stale ids are refused with 409). This is the same
//! payload `lex repair --apply --transform` takes.
//!
//! # Response (200)
//!
//! `{ ok, branch, kind, op_id, op_ids, prev_head, new_head, new_stage_id,
//!    extracted?, intent: { intent_id, session_id, unattributed } }` —
//! `op_id` is the last op emitted (the new head); `op_ids` lists all of them
//! (two for `extract_function`); `extracted` is `{ sig_id, stage_id }` of the
//! new function.
//!
//! # Errors
//!
//! 400 malformed body / missing `branch` / blank `intent.prompt`; 404 unknown
//! branch or stage; 409 stale `from_stage_id`, no-op transform, or a function
//! not on the branch head; 422 the transform did not apply (unknown node,
//! wrong node kind, ...) or the result fails the write-time gate — with the
//! diagnostics under `detail.errors`. Every refusal leaves the branch head
//! unchanged and writes no op and no intent.
//!
//! # Intent
//!
//! Attribution follows `lex publish` (#970): an absent `intent` is recorded as
//! an explicitly *unattributed* intent, never as none. A `session` that is not
//! supplied defaults, like publish's `cli-<pid>-<epoch>`, to a per-process
//! `http-<pid>-<epoch>` — so **an omitted session makes the OpId
//! non-reproducible; pin `session` for a deterministic OpId**. The resolved
//! ids are echoed in the response.

use serde::Deserialize;
use std::io::Cursor;
use tiny_http::Response;

use lex_store::{Store, StoreError};

use crate::handlers::{error_response, error_with_detail, json_response, State};

// ---- intent ---------------------------------------------------------------

/// The prompt recorded when a write declares none (#970). Deliberately not a
/// plausible-looking prompt: it must be impossible to mistake a synthesized
/// intent for one a caller supplied, and it is a fixed string so it is exactly
/// matchable (`lex recall --predicate` lists every unattributed op).
pub const UNATTRIBUTED_PROMPT: &str = "(unattributed: published without --intent-prompt)";

/// `provider/name` → `(provider, name)`. A bare name is attributed to provider
/// `cli`; `None` → `("cli", "unknown")`. The model ref feeds the content-
/// addressed IntentId, so the default must be stable, not empty.
pub fn split_model_ref(m: Option<&str>) -> (String, String) {
    match m {
        None => ("cli".to_string(), "unknown".to_string()),
        Some(s) => match s.split_once('/') {
            Some((p, n)) if !p.is_empty() && !n.is_empty() => (p.to_string(), n.to_string()),
            _ => ("cli".to_string(), s.to_string()),
        },
    }
}

/// Build (not record) the Intent for a write from its optional parts. The one
/// implementation `lex publish`, `lex ws transform` and the HTTP write
/// endpoints share, so an unattributed write looks the same whichever door it
/// came through. `default_session` is only called when `session` is `None`.
pub fn build_intent(
    prompt: Option<String>,
    model: Option<String>,
    session: Option<String>,
    issue: Option<String>,
    default_session: impl FnOnce() -> String,
) -> lex_vcs::Intent {
    let prompt = prompt.unwrap_or_else(|| UNATTRIBUTED_PROMPT.to_string());
    let (provider, name) = split_model_ref(model.as_deref());
    let intent = lex_vcs::Intent::new(
        prompt,
        session.unwrap_or_else(default_session),
        lex_vcs::ModelDescriptor { provider, name, version: None },
        None,
    );
    match issue {
        Some(id) => intent.with_issue(id),
        None => intent,
    }
}

/// The session id for an HTTP write that gave none: per-process, like
/// publish's `cli-<pid>-<epoch>`, and for the same reason (a constant default
/// would collapse every anonymous write into one bogus session).
pub fn default_http_session() -> String {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("http-{}-{started}", std::process::id())
}

/// The wire form of an intent: every field optional.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentSpec {
    pub prompt: Option<String>,
    /// `provider/name` (bare `name` ⇒ provider `cli`), as `--intent-model`.
    pub model: Option<String>,
    pub session: Option<String>,
    pub issue_id: Option<String>,
}

impl IntentSpec {
    /// Resolve to an Intent. A supplied-but-blank prompt is refused: an empty
    /// string would pass for attribution while saying nothing (#970).
    pub fn into_intent(
        self,
        default_session: impl FnOnce() -> String,
    ) -> Result<lex_vcs::Intent, String> {
        if let Some(p) = &self.prompt {
            if p.trim().is_empty() {
                return Err("intent.prompt must not be blank (omit it to record the write as unattributed)".into());
            }
        }
        Ok(build_intent(self.prompt, self.model, self.session, self.issue_id, default_session))
    }
}

// ---- the transform --------------------------------------------------------

/// A typed transform request; the JSON shape matches `lex repair --transform`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransformSpec {
    ReplaceMatchArm {
        from_stage_id: String,
        match_node: String,
        arm_index: usize,
        new_body: lex_ast::CExpr,
    },
    RenameLocal {
        from_stage_id: String,
        let_node: String,
        new_name: String,
    },
    InlineLet {
        from_stage_id: String,
        let_node: String,
    },
    ExtractFunction {
        from_stage_id: String,
        expr_node: String,
        spec: ExtractSpec,
    },
}

/// Wire form of `lex_ast::ExtractFnSpec`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractSpec {
    pub name: String,
    #[serde(default)]
    pub type_params: Vec<String>,
    pub params: Vec<lex_ast::Param>,
    pub return_type: lex_ast::TypeExpr,
    #[serde(default)]
    pub effects: Vec<lex_ast::Effect>,
}

impl TransformSpec {
    pub fn kind(&self) -> &'static str {
        match self {
            TransformSpec::ReplaceMatchArm { .. } => "replace_match_arm",
            TransformSpec::RenameLocal { .. } => "rename_local",
            TransformSpec::InlineLet { .. } => "inline_let",
            TransformSpec::ExtractFunction { .. } => "extract_function",
        }
    }

    fn source_stage_id(&self) -> &str {
        match self {
            TransformSpec::ReplaceMatchArm { from_stage_id, .. }
            | TransformSpec::RenameLocal { from_stage_id, .. }
            | TransformSpec::InlineLet { from_stage_id, .. }
            | TransformSpec::ExtractFunction { from_stage_id, .. } => from_stage_id,
        }
    }
}

/// What a successful transform landed.
#[derive(Debug, Clone)]
pub struct Applied {
    pub branch: String,
    pub kind: &'static str,
    pub op_ids: Vec<lex_vcs::OpId>,
    pub prev_head: Option<lex_vcs::OpId>,
    pub new_head: Option<lex_vcs::OpId>,
    /// The rewritten source stage (the head's stage for the source sig).
    pub new_stage_id: Option<String>,
    /// `extract_function`: `(sig_id, stage_id)` of the new function.
    pub extracted: Option<(String, String)>,
    pub intent_id: String,
    pub session_id: String,
    pub unattributed: bool,
}

impl Applied {
    /// The JSON both the HTTP response and `lex ws transform` emit.
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "ok": true,
            "branch": self.branch,
            "kind": self.kind,
            "op_id": self.op_ids.last(),
            "op_ids": self.op_ids,
            "prev_head": self.prev_head,
            "new_head": self.new_head,
            "new_stage_id": self.new_stage_id,
            "intent": {
                "intent_id": self.intent_id,
                "session_id": self.session_id,
                "unattributed": self.unattributed,
            },
        });
        if let Some((sig, stage)) = &self.extracted {
            v["extracted"] = serde_json::json!({ "sig_id": sig, "stage_id": stage });
        }
        v
    }
}

/// Apply `spec` to `branch` through the store's gated apply path, attributing
/// every op to `intent`. The one function `POST /v1/transform` and
/// `lex ws transform` share.
///
/// `branch` is an explicit argument by design — this never consults
/// [`Store::current_branch`]. On any `Err` the branch head is unchanged and no
/// op or intent was written (a stage the transform produced may remain in the
/// content-addressed store; it is unreferenced and idempotent).
pub fn apply_transform(
    store: &Store,
    branch: &str,
    spec: &TransformSpec,
    intent: &lex_vcs::Intent,
) -> Result<Applied, StoreError> {
    // `list_branches` reads directory entries, so a name is only ever used to
    // build a path after matching one of them (no traversal through `branch`).
    if !store.list_branches()?.iter().any(|b| b == branch) {
        return Err(StoreError::UnknownBranch(branch.to_string()));
    }

    // The transform must start from the stage the head binds to the sig. The
    // store only checks the sig is on the head, so an older stage of the same
    // function would otherwise be transformed and swapped in silently.
    let from = spec.source_stage_id();
    let from_ast = store.get_ast(from)?;
    if let Some(sig) = lex_ast::sig_id(&from_ast) {
        let head = store.branch_head(branch)?;
        match head.get(&sig) {
            Some(cur) if cur != from => {
                return Err(StoreError::InvalidTransition(format!(
                    "stale from_stage_id `{from}`: branch `{branch}` head binds sig `{sig}` to `{cur}`"
                )));
            }
            _ => {}
        }
    }

    let prev_head = store.get_branch(branch)?.and_then(|b| b.head_op);
    let node = |s: &str| lex_ast::NodeId(s.to_string());
    let op_ids = match spec {
        TransformSpec::ReplaceMatchArm { from_stage_id, match_node, arm_index, new_body } => {
            vec![store.apply_replace_match_arm_with_intent(
                branch,
                from_stage_id,
                &node(match_node),
                *arm_index,
                new_body.clone(),
                Some(intent),
            )?]
        }
        TransformSpec::RenameLocal { from_stage_id, let_node, new_name } => {
            vec![store.apply_rename_local_with_intent(
                branch,
                from_stage_id,
                &node(let_node),
                new_name,
                Some(intent),
            )?]
        }
        TransformSpec::InlineLet { from_stage_id, let_node } => {
            vec![store.apply_inline_let_with_intent(
                branch,
                from_stage_id,
                &node(let_node),
                Some(intent),
            )?]
        }
        TransformSpec::ExtractFunction { from_stage_id, expr_node, spec } => {
            let (add, modify) = store.apply_extract_function_with_intent(
                branch,
                from_stage_id,
                &node(expr_node),
                lex_ast::ExtractFnSpec {
                    name: spec.name.clone(),
                    type_params: spec.type_params.clone(),
                    params: spec.params.clone(),
                    return_type: spec.return_type.clone(),
                    effects: spec.effects.clone(),
                },
                Some(intent),
            )?;
            vec![add, modify]
        }
    };

    let new_head = store.get_branch(branch)?.and_then(|b| b.head_op);
    let new_stage_id = lex_ast::sig_id(&from_ast)
        .and_then(|sig| store.branch_head(branch).ok().and_then(|h| h.get(&sig).cloned()));
    let extracted = match spec {
        TransformSpec::ExtractFunction { .. } => {
            let log = lex_vcs::OpLog::open(store.root())?;
            match op_ids.first().and_then(|id| log.get(id).ok().flatten()) {
                Some(rec) => match rec.op.kind {
                    lex_vcs::OperationKind::AddFunction { sig_id, stage_id, .. } => {
                        Some((sig_id, stage_id))
                    }
                    _ => None,
                },
                None => None,
            }
        }
        _ => None,
    };
    Ok(Applied {
        branch: branch.to_string(),
        kind: spec.kind(),
        op_ids,
        prev_head,
        new_head,
        new_stage_id,
        extracted,
        intent_id: intent.intent_id.clone(),
        session_id: intent.session_id.clone(),
        unattributed: intent.prompt == UNATTRIBUTED_PROMPT,
    })
}

// ---- HTTP -----------------------------------------------------------------

#[derive(Deserialize)]
struct TransformReq {
    branch: Option<String>,
    #[serde(default)]
    intent: Option<IntentSpec>,
    transform: TransformSpec,
}

/// Map a store error from the transform path to a response. Refusals are
/// 4xx with the store's own message; everything else falls through to the
/// shared write-error mapping (409/503 contention, budget, 500).
pub(crate) fn transform_error_response(err: StoreError) -> Response<Cursor<Vec<u8>>> {
    match err {
        StoreError::UnknownBranch(_) | StoreError::UnknownStage(_) | StoreError::UnknownSig(_) => {
            error_response(404, err.to_string())
        }
        StoreError::TransformError(ref e) => error_with_detail(
            422,
            format!("transform failed: {e}"),
            serde_json::json!({ "kind": "transform_error", "message": e.to_string() }),
        ),
        StoreError::TypeError(ref errs) => error_with_detail(
            422,
            "type errors after transform",
            serde_json::json!({
                "kind": "type_errors",
                "errors": serde_json::to_value(errs).unwrap_or_default(),
            }),
        ),
        StoreError::InvalidTransition(_) => error_response(409, err.to_string()),
        other => crate::handlers::write_error_response("transform", other),
    }
}

/// `POST /v1/transform` — see the module docs.
pub(crate) fn transform_handler(state: &State, body: &str) -> Response<Cursor<Vec<u8>>> {
    let req: TransformReq = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let Some(branch) = req.branch.filter(|b| !b.is_empty()) else {
        return error_response(
            400,
            "bad request: `branch` is required (the server's current branch is never assumed)",
        );
    };
    let intent = match req.intent.unwrap_or_default().into_intent(default_http_session) {
        Ok(i) => i,
        Err(e) => return error_response(400, format!("bad request: {e}")),
    };
    let store = state.store.lock().unwrap();
    match apply_transform(&store, &branch, &req.transform, &intent) {
        Ok(applied) => json_response(200, &applied.to_json()),
        Err(e) => transform_error_response(e),
    }
}
