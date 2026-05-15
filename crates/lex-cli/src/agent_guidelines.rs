//! `lex agent-guidelines` — emit the canonical AI-agent authoring
//! contract (`docs/AGENT_GUIDELINES.md`) baked into the binary at
//! compile time.
//!
//! Why a subcommand instead of "just open the file": the guidelines
//! travel with the toolchain version, so an agent running against a
//! pinned `lex` binary gets the rules that match the type checker,
//! stdlib, and effect kinds for *that* version. Reading the file on
//! disk via a URL or path is brittle in CI / sandboxed agent
//! environments; `lex agent-guidelines > AGENTS.md` works anywhere
//! the toolchain works.
//!
//! The output is the file verbatim. JSON mode wraps it in the ACLI
//! success envelope so agents that only consume JSON still get it.

use acli::output::OutputFormat;
use anyhow::Result;
use serde_json::json;

/// The doc, embedded at compile time. Bumps with every release.
const GUIDELINES_MD: &str = include_str!("../../../docs/AGENT_GUIDELINES.md");

pub fn cmd_agent_guidelines(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // Optional flag for downstream tooling that wants to know which
    // toolchain version emitted the doc without parsing markdown.
    if args.iter().any(|a| a == "--version-only") {
        match fmt {
            OutputFormat::Json => {
                let env = json!({
                    "ok": true,
                    "command": "agent-guidelines",
                    "data": { "lex_version": env!("CARGO_PKG_VERSION") },
                });
                println!("{}", serde_json::to_string(&env).unwrap());
            }
            _ => println!("{}", env!("CARGO_PKG_VERSION")),
        }
        return Ok(());
    }

    match fmt {
        OutputFormat::Json => {
            let env = json!({
                "ok": true,
                "command": "agent-guidelines",
                "data": {
                    "lex_version": env!("CARGO_PKG_VERSION"),
                    "format": "markdown",
                    "content": GUIDELINES_MD,
                },
            });
            println!("{}", serde_json::to_string(&env).unwrap());
        }
        OutputFormat::Text | OutputFormat::Table => {
            // Print verbatim; no decoration. Suitable for redirection:
            //   lex agent-guidelines > AGENTS.md
            print!("{GUIDELINES_MD}");
        }
    }
    Ok(())
}
