//! Minimal stdio MCP (Model Context Protocol) client for the
//! `agent.call_mcp` builtin (#185). Spawns the named MCP server
//! as a subprocess, completes the `initialize` handshake, then
//! forwards a `tools/call` request and returns the result.
//!
//! Scope:
//!
//! - One client per call (spawn-per-call). MCP servers are cheap
//!   to start; per-call spawning keeps the implementation
//!   stateless. A connection cache is a clear v2 optimization
//!   once benchmarks show it matters.
//! - stdio transport only. Future transports (TCP / SSE / HTTP)
//!   can branch on a URL prefix in `command` later.
//! - No auth; the issue body explicitly defers credential
//!   handling to a follow-up. Don't expose this builtin to
//!   untrusted code paths until that's done.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

pub struct McpClient {
    child: std::process::Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    /// Spawn the MCP server described by `command_line` (whitespace-
    /// separated argv), perform the JSON-RPC `initialize` handshake,
    /// and return a ready client. Caller is expected to drop the
    /// client when finished — `Drop` reaps the subprocess.
    pub fn spawn(command_line: &str) -> Result<Self, String> {
        let parts: Vec<&str> = command_line.split_whitespace().collect();
        let cmd = parts.first()
            .ok_or_else(|| "mcp.spawn: empty command".to_string())?;
        let mut child = Command::new(cmd)
            .args(&parts[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("mcp.spawn `{cmd}`: {e}"))?;
        let stdin = child.stdin.take()
            .ok_or_else(|| "mcp.spawn: no stdin".to_string())?;
        let stdout = BufReader::new(child.stdout.take()
            .ok_or_else(|| "mcp.spawn: no stdout".to_string())?);
        let mut client = Self { child, stdin, stdout, next_id: 0 };
        client.initialize()?;
        Ok(client)
    }

    fn next_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)
            .map_err(|e| format!("mcp.rpc: serialize: {e}"))?;
        self.stdin.write_all(line.as_bytes())
            .map_err(|e| format!("mcp.rpc: write: {e}"))?;
        self.stdin.write_all(b"\n")
            .map_err(|e| format!("mcp.rpc: write: {e}"))?;
        self.stdin.flush()
            .map_err(|e| format!("mcp.rpc: flush: {e}"))?;
        // The server may emit notifications before the response;
        // skip lines whose id doesn't match.
        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf)
                .map_err(|e| format!("mcp.rpc: read: {e}"))?;
            if n == 0 {
                return Err("mcp.rpc: server closed stdout".into());
            }
            let resp: Value = serde_json::from_str(buf.trim())
                .map_err(|e| format!("mcp.rpc: parse `{}`: {e}",
                    buf.trim().chars().take(120).collect::<String>()))?;
            // Skip notifications (no `id` field).
            if resp.get("id").is_none() { continue; }
            if resp["id"] != json!(id) { continue; }
            if let Some(err) = resp.get("error") {
                return Err(format!("mcp.rpc {method}: {err}"));
            }
            return Ok(resp.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn initialize(&mut self) -> Result<(), String> {
        // Minimal client capabilities — Phase 1 only needs
        // synchronous tool calls. `protocolVersion` matches the
        // version `lex-api`'s server reports.
        self.rpc("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "lex-runtime", "version": env!("CARGO_PKG_VERSION") },
        }))?;
        Ok(())
    }

    /// Send `tools/call` for the named tool with the supplied
    /// JSON arguments. Returns the server's `result` field as
    /// JSON; tool-side errors come back as `Err`.
    pub fn call_tool(&mut self, name: &str, args: Value) -> Result<Value, String> {
        self.rpc("tools/call", json!({
            "name": name,
            "arguments": args,
        }))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Best-effort reap: if the server hasn't already exited
        // on stdin EOF, kill it. Don't propagate errors — Drop
        // can't fail meaningfully and the process either exits
        // or gets reaped by the OS.
        let _ = self.child.kill();
        // Avoid zombies: wait briefly. If the server is misbehaving
        // we don't want Drop to block forever.
        let _ = self.child.wait();
        let _ = Duration::from_millis(0);
    }
}
