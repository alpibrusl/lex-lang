//! `lex serve`: the local HTTP API over a store.

use super::*;

pub(super) fn cmd_serve(args: &[String]) -> Result<()> {
    let mut port: u16 = 4040;
    let mut store_root: Option<std::path::PathBuf> = None;
    let mut mcp = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--port needs value"))?;
                port = v.parse().context("--port must be u16")?;
                i += 2;
            }
            "--store" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("--store needs path"))?;
                store_root = Some(std::path::PathBuf::from(v));
                i += 2;
            }
            "--mcp" => {
                mcp = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let store_root = store_root.unwrap_or_else(default_store_root);
    if mcp {
        // MCP transport is stdio; --port is irrelevant. The host
        // (Claude Code, Cursor, etc.) spawns this subprocess and
        // pipes JSON-RPC over stdin/stdout.
        eprintln!("lex MCP server (stdio) — store: {}", store_root.display());
        return lex_api::serve_mcp_stdio(store_root);
    }
    eprintln!("lex agent API listening on http://127.0.0.1:{port}");
    eprintln!("store: {}", store_root.display());
    lex_api::serve(port, store_root)
}
