//! Model Context Protocol server (#171).
//!
//! Wraps the JSON API at `/v1/*` as MCP tools so any MCP-speaking
//! host (Claude Code, Cursor, Codex, etc.) can invoke Lex actions
//! natively. Stdio transport, JSON-RPC 2.0, hand-rolled — no SDK
//! dependency, ~250 lines.
//!
//! Run with `lex serve --mcp` (sticks to the same `State` shape
//! as the HTTP server, just speaks a different protocol on
//! stdin/stdout instead of HTTP).
//!
//! Tools shipped in v1:
//!
//! * `lex_check` — POST /v1/check
//! * `lex_publish` — POST /v1/publish
//! * `lex_run` — POST /v1/run (effect policy passes through)
//! * `lex_stage_get` — GET /v1/stage/<id>
//! * `lex_stage_attestations` — GET /v1/stage/<id>/attestations
//!
//! Merge endpoints + trace/diff/replay land in v2; the v1 set
//! covers the common agent loop (check → publish → run → read
//! attestations).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;

use crate::handlers::State;

/// JSON-RPC 2.0 envelope. We deserialize into `Value` first so a
/// missing `id` (notification) doesn't error out — notifications
/// are valid in MCP and shouldn't crash the loop.
#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Run the MCP server. Reads JSON-RPC requests from stdin (one
/// per line), writes responses to stdout. Stops cleanly on EOF.
///
/// `eprintln!` is fine for diagnostics — MCP hosts read stdout
/// for the protocol and surface stderr in their UI separately.
pub fn serve_mcp(state: Arc<State>) -> std::io::Result<()> {
    eprintln!("lex MCP server ready (stdio); v1 tools: lex_check, lex_publish, lex_run, lex_stage_get, lex_stage_attestations");
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => {
                if let Some(resp) = dispatch(&state, req) {
                    let body = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into());
                    writeln!(out, "{body}")?;
                    out.flush()?;
                }
                // None = notification (no `id`); MCP says don't reply.
            }
            Err(e) => {
                // Parse error per JSON-RPC spec: id is null.
                let resp = RpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(RpcError {
                        code: -32700,
                        message: format!("parse error: {e}"),
                        data: None,
                    }),
                };
                writeln!(out, "{}", serde_json::to_string(&resp).unwrap())?;
                out.flush()?;
            }
        }
    }
    Ok(())
}

/// Returns `Some(response)` for requests that need a reply, `None`
/// for notifications. The dispatch is small enough to be flat.
fn dispatch(state: &State, req: RpcRequest) -> Option<RpcResponse> {
    let id = req.id?;          // notification → drop
    let method = req.method.as_str();
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "lex",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(state, &req.params),
        // Hosts ping; respond with empty object.
        "ping" => Ok(json!({})),
        other => Err(RpcError {
            code: -32601,
            message: format!("method not found: {other}"),
            data: None,
        }),
    };
    Some(match result {
        Ok(v) => RpcResponse { jsonrpc: "2.0", id, result: Some(v), error: None },
        Err(e) => RpcResponse { jsonrpc: "2.0", id, result: None, error: Some(e) },
    })
}

/// MCP `tools/list` response. Each entry is `{name, description,
/// inputSchema}`. Schemas mirror the JSON request bodies the
/// HTTP handlers already accept.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "lex_check",
            "description": "Type-check a Lex source string. Returns ok or a list of TypeErrors with structured detail.",
            "inputSchema": {
                "type": "object",
                "properties": { "source": { "type": "string" } },
                "required": ["source"]
            }
        },
        {
            "name": "lex_publish",
            "description": "Publish a Lex source to the store. Type-check gate runs first; rejected sources don't advance the branch head. Returns the typed ops produced.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string" },
                    "activate": { "type": "boolean", "default": false }
                },
                "required": ["source"]
            }
        },
        {
            "name": "lex_run",
            "description": "Execute a Lex function under an effect policy. Pure programs run with no policy; effectful ones need allow_effects / allow_fs_read / allow_fs_write / allow_net_host grants.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string" },
                    "fn": { "type": "string" },
                    "args": { "type": "array", "items": {} },
                    "policy": {
                        "type": "object",
                        "properties": {
                            "allow_effects": { "type": "array", "items": { "type": "string" } },
                            "allow_fs_read": { "type": "array", "items": { "type": "string" } },
                            "allow_fs_write": { "type": "array", "items": { "type": "string" } },
                            "budget": { "type": "integer" }
                        }
                    }
                },
                "required": ["source", "fn"]
            }
        },
        {
            "name": "lex_stage_get",
            "description": "Fetch a stage's metadata + canonical AST + status by stage_id (lowercase-hex SHA-256).",
            "inputSchema": {
                "type": "object",
                "properties": { "stage_id": { "type": "string" } },
                "required": ["stage_id"]
            }
        },
        {
            "name": "lex_stage_attestations",
            "description": "List every attestation persisted against a stage (TypeCheck / Spec / Examples / DiffBody / EffectAudit / SandboxRun). Newest-first.",
            "inputSchema": {
                "type": "object",
                "properties": { "stage_id": { "type": "string" } },
                "required": ["stage_id"]
            }
        }
    ])
}

/// Dispatch a `tools/call` request. The MCP shape is `{name,
/// arguments}`; we route on `name` and forward `arguments` as
/// the JSON body the corresponding HTTP handler already accepts
/// — keeps the two surfaces in lockstep without duplicating
/// business logic.
fn call_tool(state: &State, params: &Value) -> Result<Value, RpcError> {
    let name = params.get("name").and_then(|v| v.as_str()).ok_or_else(|| RpcError {
        code: -32602, message: "tools/call: missing `name`".into(), data: None,
    })?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    // The HTTP handlers all take `&str` body and return a typed
    // `Response<Cursor<Vec<u8>>>`. We extract the body bytes +
    // status, then wrap into MCP's `content` shape: success →
    // text content with the JSON body; error → isError + same
    // content. Lets the host see the structured error envelope
    // the JSON API already produces.
    let body = serde_json::to_string(&args).unwrap_or_else(|_| "{}".into());
    let (status, response_body): (u16, String) = match name {
        "lex_check" => http_to_string(crate::handlers::check_handler(&body)),
        "lex_publish" => http_to_string(crate::handlers::publish_handler(state, &body)),
        "lex_run" => http_to_string(crate::handlers::run_handler(state, &body, false)),
        "lex_stage_get" => {
            let id = args.get("stage_id").and_then(|v| v.as_str()).ok_or_else(|| RpcError {
                code: -32602, message: "lex_stage_get: missing stage_id".into(), data: None,
            })?;
            http_to_string(crate::handlers::stage_handler(state, id))
        }
        "lex_stage_attestations" => {
            let id = args.get("stage_id").and_then(|v| v.as_str()).ok_or_else(|| RpcError {
                code: -32602, message: "lex_stage_attestations: missing stage_id".into(), data: None,
            })?;
            http_to_string(crate::handlers::stage_attestations_handler(state, id))
        }
        other => return Err(RpcError {
            code: -32602, message: format!("unknown tool: {other}"), data: None,
        }),
    };

    let is_error = !(200..300).contains(&status);
    Ok(json!({
        "content": [{ "type": "text", "text": response_body }],
        "isError": is_error,
    }))
}

/// Drain an HTTP `Response` (the type all handlers return) into
/// `(status, body_string)` so the MCP wrapper can repackage it.
fn http_to_string(
    resp: tiny_http::Response<std::io::Cursor<Vec<u8>>>,
) -> (u16, String) {
    // tiny_http's Response doesn't expose body bytes after construction
    // without consuming the reader; we use our caller-side knowledge
    // that handlers built the body from a Vec<u8>. The status code
    // is on `status_code()`. For the body we re-render via the
    // public iterator the response exposes.
    let status = resp.status_code().0;
    let mut buf = Vec::new();
    let mut reader = resp.into_reader();
    let _ = std::io::copy(&mut reader, &mut buf);
    let body = String::from_utf8(buf).unwrap_or_default();
    (status, body)
}
