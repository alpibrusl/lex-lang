//! End-to-end tests for `lex serve --mcp` (#171).
//!
//! Spawn the binary with `--mcp`, write JSON-RPC requests to its
//! stdin one line at a time, read responses from stdout, assert
//! shape. Same approach a real MCP host (Claude Code, Cursor)
//! uses, just without the host part.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

/// Spawn the MCP server pointed at an ephemeral store. Returns
/// the child + a stdin handle + a stdout reader. The child stays
/// alive until dropped (Drop kills the process).
struct McpProcess {
    child: Child,
    _tmp: tempfile::TempDir,
}

impl McpProcess {
    fn start() -> Self {
        let tmp = tempdir().unwrap();
        let child = Command::new(lex_bin())
            .args(["serve", "--mcp", "--store", tmp.path().to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lex serve --mcp");
        Self { child, _tmp: tmp }
    }

    /// Send one JSON-RPC request, return the parsed response.
    fn round_trip(&mut self, req: serde_json::Value) -> serde_json::Value {
        let line = req.to_string();
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{line}").expect("write stdin");
        stdin.flush().expect("flush");

        let stdout = self.child.stdout.as_mut().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let mut buf = String::new();
        reader.read_line(&mut buf).expect("read line");
        serde_json::from_str(&buf).unwrap_or_else(|e| {
            panic!("parse mcp response {buf:?}: {e}");
        })
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_initialize_returns_server_info() {
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05" }
    }));
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    let result = &resp["result"];
    assert_eq!(result["serverInfo"]["name"], "lex");
    assert!(result["protocolVersion"].is_string());
}

#[test]
fn mcp_tools_list_advertises_v1_tools() {
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list"
    }));
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter()
        .map(|t| t["name"].as_str().unwrap_or(""))
        .collect();
    for expected in &["lex_check", "lex_publish", "lex_run", "lex_stage_get", "lex_stage_attestations"] {
        assert!(names.contains(expected), "missing tool {expected}; got {names:?}");
    }
    // Each tool must have an inputSchema; otherwise the host
    // can't render argument forms.
    for t in tools {
        assert!(t["inputSchema"].is_object(), "tool missing inputSchema: {t}");
    }
}

#[test]
fn mcp_call_check_round_trips() {
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {
            "name": "lex_check",
            "arguments": { "source": "fn add(x :: Int, y :: Int) -> Int { x + y }" }
        }
    }));
    assert_eq!(resp["result"]["isError"], false);
    let text = resp["result"]["content"][0]["text"].as_str().expect("text content");
    assert!(text.contains("\"ok\":true"), "expected ok envelope, got {text:?}");
}

#[test]
fn mcp_call_check_surfaces_type_errors_as_is_error() {
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {
            "name": "lex_check",
            "arguments": { "source": "fn bad(x :: Int) -> Str { x }" }
        }
    }));
    // 422 → isError true; the structured TypeError JSON sits in
    // the text content for the agent to parse.
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("type_mismatch"), "expected TypeMismatch in body: {text}");
}

#[test]
fn mcp_publish_then_read_attestations() {
    // The agent loop in miniature: publish, then ask for the
    // stage's attestations. Should see the auto-emitted TypeCheck.
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call",
        "params": {
            "name": "lex_publish",
            "arguments": {
                "source": "fn fac(n :: Int) -> Int { 1 }\n",
                "activate": true
            }
        }
    }));
    assert_eq!(resp["result"]["isError"], false);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let pub_body: serde_json::Value = serde_json::from_str(text).unwrap();
    let stage_id = pub_body["ops"][0]["kind"]["stage_id"].as_str().expect("stage_id");

    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 6, "method": "tools/call",
        "params": {
            "name": "lex_stage_attestations",
            "arguments": { "stage_id": stage_id }
        }
    }));
    assert_eq!(resp["result"]["isError"], false);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("type_check"), "expected TypeCheck attestation: {text}");
    assert!(text.contains("passed"), "expected passed result: {text}");
}

#[test]
fn mcp_unknown_tool_returns_error() {
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "lex_nonexistent", "arguments": {} }
    }));
    assert!(resp["error"].is_object(), "expected JSON-RPC error: {resp}");
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn mcp_unknown_method_returns_error() {
    let mut p = McpProcess::start();
    let resp = p.round_trip(serde_json::json!({
        "jsonrpc": "2.0", "id": 8, "method": "nonexistent/method"
    }));
    assert_eq!(resp["error"]["code"], -32601);
}
