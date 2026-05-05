//! M8: agent API server. Spec §12.3.
//!
//! HTTP/JSON server exposing the same operations as the CLI. The server
//! is stateful (it owns a `Store` instance) so agents don't pay sandbox
//! startup cost per request.
//!
//! Endpoints:
//!   POST /v1/parse           { source } → CanonicalAst | [SyntaxError]
//!   POST /v1/check           { source } → { ok: true } | [TypeError]
//!   POST /v1/publish         { source, activate? } → [{ stage_id, sig_id, status }]
//!   GET  /v1/stage/<id>      → { metadata, ast, status }
//!   GET  /v1/stage/<id>/attestations → { attestations: [Attestation] }
//!   POST /v1/run             { source, fn, args, policy } → { run_id, output | error }
//!   GET  /v1/trace/<run_id>  → TraceTree
//!   POST /v1/replay          { source, fn, args, policy, overrides } → { run_id, output | error }
//!   GET  /v1/diff?a=&b=      → Divergence | { divergence: null }
//!   POST /v1/merge/start              { src_branch, dst_branch } → { merge_id, conflicts, ... }
//!   POST /v1/merge/<id>/resolve       { resolutions: [...] } → { verdicts, remaining_conflicts }
//!   POST /v1/merge/<id>/commit        → { new_head_op, dst_branch } | 422 conflicts remaining
//!   GET  /v1/health          → { ok: true }
//!
//! Web (lex-tea v1, read-only HTML over the JSON API):
//!   GET  /                      → branch list
//!   GET  /web/branch/<name>     → fns on a branch
//!   GET  /web/stage/<id>        → stage info + attestation trail

pub mod handlers;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

pub fn serve(port: u16, store_root: PathBuf) -> anyhow::Result<()> {
    let server = tiny_http::Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("bind failed: {e}"))?;
    let state = Arc::new(handlers::State::open(store_root)?);
    serve_on(server, state);
    Ok(())
}

/// Test/embedded entry: takes an already-bound `Server` and runs until it
/// stops accepting requests. Returns immediately when the `Server` is
/// dropped on another thread.
pub fn serve_on(server: tiny_http::Server, state: Arc<handlers::State>) {
    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        let _ = handlers::handle(state, request);
    }
}
