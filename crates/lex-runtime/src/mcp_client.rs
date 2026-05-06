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

// ---- LRU connection cache (#197) --------------------------------

/// Bounded cache of `McpClient` instances keyed by the
/// command-line string. Keeps a Vec of `(key, client)` pairs
/// in usage order — most-recently-used at the back. On cache
/// miss past `cap`, the front (oldest) entry is dropped.
///
/// Why a Vec rather than a `HashMap` + linked list: cap is
/// small (16 by default) so linear scan is cheaper than the
/// pointer chase, and a Vec lets us own the Clients directly
/// instead of through `RefCell`/`Arc`.
///
/// Subprocess death is detected lazily: when `call_tool` fails
/// the offending entry is dropped. Next call to the same
/// server respawns. A handler that sits idle long enough for
/// upstream MCP servers to be killed by ops will see one
/// `Err` per server before recovering.
pub struct McpClientCache {
    entries: Vec<(String, McpClient)>,
    cap: usize,
}

impl McpClientCache {
    pub fn with_capacity(cap: usize) -> Self {
        Self { entries: Vec::with_capacity(cap), cap }
    }

    /// Send a `tools/call` to the named server, spawning the
    /// subprocess on cache miss and reusing it on hit. Returns
    /// the server's `result` JSON or an error message; on error,
    /// the offending client is dropped so the next call respawns.
    pub fn call(
        &mut self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Hit: move the entry to the back (mark MRU) and call.
        if let Some(idx) = self.entries.iter().position(|(k, _)| k == server) {
            let (key, mut client) = self.entries.remove(idx);
            match client.call_tool(tool, args) {
                Ok(v) => {
                    self.entries.push((key, client));
                    Ok(v)
                }
                Err(e) => {
                    // Dropping `client` reaps the subprocess.
                    Err(e)
                }
            }
        } else {
            // Miss: spawn, evict if at capacity, push.
            let mut client = McpClient::spawn(server)?;
            let result = client.call_tool(tool, args);
            if result.is_ok() {
                if self.entries.len() >= self.cap && !self.entries.is_empty() {
                    self.entries.remove(0);
                }
                self.entries.push((server.to_string(), client));
            }
            result
        }
    }

    /// Number of cached subprocesses. Useful for tests and
    /// observability; not on the hot path.
    pub fn len(&self) -> usize { self.entries.len() }

    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}

impl Default for McpClientCache {
    fn default() -> Self { Self::with_capacity(16) }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// Smoke test that the cache structure works without
    /// actually spawning subprocesses (which would require a
    /// real MCP server). Tests against the real `lex serve --mcp`
    /// fixture live in `tests/std_agent_mcp_client.rs`.
    #[test]
    fn empty_cache_starts_at_zero_with_configured_cap() {
        let c = McpClientCache::with_capacity(4);
        assert_eq!(c.len(), 0);
        assert!(c.is_empty());
        assert_eq!(c.cap, 4);
    }
}
