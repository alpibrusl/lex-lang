//! ACLI integration — register every `lex` subcommand with the
//! [acli](https://github.com/alpibrusl/acli) Rust SDK so the binary
//! is self-describing to any LLM agent (Claude, Codex, Gemini,
//! Qwen, ...) via a single spec instead of one bespoke skill per
//! agent platform.
//!
//! What ships:
//!
//! - `lex introspect [--output text|json]` — full command tree as
//!   JSON per ACLI §1.2; agents read this once and learn the surface.
//! - `lex skill [--output text|json]` — markdown + YAML frontmatter
//!   per agentskills.io; suitable for `cp <(lex skill) SKILL.md`.
//! - `lex version [--output text|json]` — name + version + acli
//!   spec version.
//!
//! `--output` is also accepted as a top-level flag *before* the
//! subcommand name (`lex --output json introspect`); it's consumed
//! by `parse_output_format` in `main.rs` and passed through here.
//!
//! Implementation note: we register `CommandInfo` *manually* — the
//! `acli` crate's `acli_args!` / `register::<C>()` path assumes
//! commands are clap-derived structs, which `lex` is not. The
//! manual builder gives us the same JSON envelope output without
//! migrating ~1000 lines of hand-rolled arg parsing.

use acli::introspect::CommandInfo;
use acli::output::{emit, error_envelope, success_envelope, OutputFormat};
use acli::AcliApp;
use serde_json::Value;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn build_app() -> AcliApp {
    let mut app = AcliApp::new("lex", VERSION);
    for cmd in commands() {
        app.register_command(cmd);
    }
    app
}

/// Emit a successful JSON envelope when the format is JSON; otherwise
/// run the supplied text printer. Centralizes the
/// "if json then envelope, else text" branch each subcommand needs.
pub fn emit_or_text<F: FnOnce()>(cmd: &str, data: Value, fmt: &OutputFormat, text: F) {
    match fmt {
        OutputFormat::Json => {
            let env = success_envelope(cmd, data, VERSION, None, None);
            emit(&env, fmt);
        }
        OutputFormat::Text | OutputFormat::Table => text(),
    }
}

/// Emit a "dry run" envelope (exit code 9 per ACLI spec §3) for
/// state-modifying commands invoked with `--dry-run`. In text mode
/// we render the planned actions as a bullet list so a human
/// running by hand still sees what would happen.
pub fn emit_dry_run(
    cmd: &str,
    fmt: &OutputFormat,
    summary: &str,
    actions: Vec<Value>,
) {
    match fmt {
        OutputFormat::Json => {
            // Build the envelope manually — the SDK's success_envelope
            // doesn't accept a `dry_run` flag; we serialize directly.
            let env = serde_json::json!({
                "ok": true,
                "command": cmd,
                "dry_run": true,
                "planned_actions": actions,
                "data": null,
                "meta": { "duration_ms": 0, "version": VERSION },
            });
            println!("{}", serde_json::to_string_pretty(&env).unwrap());
        }
        OutputFormat::Text | OutputFormat::Table => {
            eprintln!("dry-run: {summary}");
            for a in &actions {
                eprintln!("  • {}", a);
            }
        }
    }
    std::process::exit(9);
}

/// Emit an error envelope for the top-level error path.
/// Maps anyhow's display string into ACLI's error structure;
/// the exit code is the caller's responsibility.
pub fn emit_error(cmd: &str, msg: &str, fmt: &OutputFormat, code: acli::ExitCode) {
    if matches!(fmt, OutputFormat::Json) {
        let env = error_envelope(
            cmd,
            code,
            msg,
            None,
            None,
            None,
            VERSION,
            None,
        );
        emit(&env, fmt);
    } else {
        eprintln!("error: {msg}");
    }
}

fn commands() -> Vec<CommandInfo> {
    vec![
        cmd_parse(),
        cmd_check(),
        cmd_run(),
        cmd_hash(),
        cmd_blame(),
        cmd_publish(),
        cmd_store(),
        cmd_trace(),
        cmd_replay(),
        cmd_diff(),
        cmd_serve(),
        cmd_conformance(),
        cmd_spec(),
        cmd_agent_tool(),
        cmd_tool_registry(),
        cmd_audit(),
        cmd_ast_diff(),
        cmd_ast_merge(),
        cmd_branch(),
        cmd_store_merge(),
        cmd_log(),
    ]
}

// ---- per-command metadata -----------------------------------------

fn cmd_parse() -> CommandInfo {
    CommandInfo::new("parse", "print canonical AST as JSON")
        .idempotent(true)
        .add_argument("file", "string", "path to a .lex file (or `-` for stdin)", true)
        .with_examples(vec![
            ("Parse a file", "lex parse hello.lex"),
            ("Parse stdin", "cat hello.lex | lex parse -"),
        ])
        .with_see_also(vec!["check", "hash"])
}

fn cmd_check() -> CommandInfo {
    CommandInfo::new("check", "type-check; exit 0 or print errors")
        .idempotent(true)
        .add_argument("file", "string", "path to a .lex file", true)
        .with_examples(vec![
            ("Type-check a file", "lex check hello.lex"),
            ("Check before running", "lex check app.lex && lex run app.lex main"),
        ])
        .with_see_also(vec!["parse", "run"])
}

fn cmd_run() -> CommandInfo {
    CommandInfo::new("run", "execute fn under capability policy (args parsed as JSON)")
        .idempotent(false)
        .add_argument("file", "string", "path to a .lex file", true)
        .add_argument("fn", "string", "function name to invoke", true)
        .add_argument("args", "string[]", "JSON-encoded positional args", false)
        .add_option("--allow-effects", "string", "comma-separated effect kinds to permit", None)
        .add_option("--allow-fs-read", "string", "filesystem path tree readable by fs_read", None)
        .add_option("--allow-fs-write", "string", "filesystem path tree writable by fs_write", None)
        .add_option("--allow-net-host", "string", "permit net effects to this host", None)
        .add_option("--budget", "int", "cap aggregate declared budget", None)
        .add_option("--max-steps", "int", "cap VM opcode dispatches (DoS guard)", None)
        .with_examples(vec![
            ("Run main()", "lex run app.lex main"),
            ("Run with fs read scope", "lex run --allow-fs-read /tmp app.lex load \"/tmp/x.json\""),
        ])
        .with_see_also(vec!["check", "replay", "trace"])
}

fn cmd_hash() -> CommandInfo {
    CommandInfo::new("hash", "print canonical SigId/StageId hashes for each function")
        .idempotent(true)
        .add_argument("file", "string", "path to a .lex file", true)
        .with_examples(vec![
            ("Hash a file", "lex hash app.lex"),
            ("Diff hashes across versions", "diff <(lex hash a.lex) <(lex hash b.lex)"),
        ])
        .with_see_also(vec!["publish", "ast-diff"])
}

fn cmd_blame() -> CommandInfo {
    CommandInfo::new("blame", "show each fn's stage history from the store")
        .idempotent(true)
        .add_argument("file", "string", "path to a .lex file", true)
        .add_option("--store", "string", "store root directory", None)
        .with_examples(vec![
            ("Blame a file", "lex blame app.lex"),
            ("Machine-readable", "lex --output json blame app.lex"),
        ])
        .with_see_also(vec!["hash", "publish", "store", "log"])
}

fn cmd_publish() -> CommandInfo {
    CommandInfo::new("publish", "publish each stage in a file to the store as Draft")
        .idempotent(false)
        .add_argument("file", "string", "path to a .lex file", true)
        .add_option("--store", "string", "store root directory (default: ~/.lex/store)", None)
        .add_option("--activate", "bool", "transition published stages to Active", None)
        .with_examples(vec![
            ("Publish drafts", "lex publish app.lex"),
            ("Publish + activate", "lex publish --activate app.lex"),
        ])
        .with_see_also(vec!["store", "branch"])
}

fn cmd_store() -> CommandInfo {
    let list = CommandInfo::new("list", "list SigIds in the store")
        .idempotent(true)
        .add_option("--store", "string", "store root directory", None);
    let get = CommandInfo::new("get", "print metadata + canonical AST for a StageId")
        .idempotent(true)
        .add_argument("stage", "string", "StageId hex", true)
        .add_option("--store", "string", "store root directory", None);
    let mut info = CommandInfo::new("store", "browse the content-addressed code store")
        .with_examples(vec![
            ("List sigs", "lex store list"),
            ("Get a stage", "lex store get abc123..."),
        ])
        .with_see_also(vec!["publish", "branch", "store-merge"]);
    info.subcommands = vec![list, get];
    info
}

fn cmd_trace() -> CommandInfo {
    CommandInfo::new("trace", "print a saved execution trace tree as JSON")
        .idempotent(true)
        .add_argument("run_id", "string", "run identifier", true)
        .with_examples(vec![
            ("Inspect a trace", "lex trace 2024-01-15-abc"),
        ])
        .with_see_also(vec!["replay", "diff"])
}

fn cmd_replay() -> CommandInfo {
    CommandInfo::new("replay", "re-execute with effect overrides keyed by NodeId")
        .idempotent(false)
        .add_argument("run_id", "string", "run identifier", true)
        .add_argument("file", "string", "source file", true)
        .add_argument("fn", "string", "function name", true)
        .add_argument("args", "string[]", "JSON-encoded positional args", false)
        .add_option("--override", "string", "NODE=JSON override (repeatable)", None)
        .with_examples(vec![
            ("Replay verbatim", "lex replay 2024-01-15-abc app.lex main"),
            ("Replay with override", "lex replay 2024-01-15-abc app.lex main --override 7=42"),
        ])
        .with_see_also(vec!["run", "trace", "diff"])
}

fn cmd_diff() -> CommandInfo {
    CommandInfo::new("diff", "first NodeId where two execution traces diverge")
        .idempotent(true)
        .add_argument("run_a", "string", "first run id", true)
        .add_argument("run_b", "string", "second run id", true)
        .with_examples(vec![
            ("Compare two runs", "lex diff run-1 run-2"),
        ])
        .with_see_also(vec!["replay", "trace", "ast-diff"])
}

fn cmd_serve() -> CommandInfo {
    CommandInfo::new("serve", "start the agent API HTTP server")
        .idempotent(false)
        .add_option("--port", "int", "TCP port (default: 7000)", None)
        .add_option("--store", "string", "store root directory", None)
        .with_examples(vec![
            ("Start with defaults", "lex serve"),
            ("Pin port + store", "lex serve --port 8080 --store /var/lex"),
        ])
        .with_see_also(vec!["tool-registry"])
}

fn cmd_conformance() -> CommandInfo {
    CommandInfo::new("conformance", "run all JSON test descriptors under a directory")
        .idempotent(true)
        .add_argument("dir", "string", "directory of conformance descriptors", true)
        .with_examples(vec![
            ("Run shipped suite", "lex conformance crates/conformance/tests/data"),
        ])
        .with_see_also(vec!["check", "spec"])
}

fn cmd_spec() -> CommandInfo {
    let check = CommandInfo::new("check", "check a Spec against a Lex source")
        .idempotent(true)
        .add_argument("spec", "string", "spec file", true)
        .add_option("--source", "string", "source file to verify against", None);
    let smt = CommandInfo::new("smt", "emit SMT-LIB for external Z3")
        .idempotent(true)
        .add_argument("spec", "string", "spec file", true);
    let mut info = CommandInfo::new("spec", "Spec proof checker (randomized + SMT-LIB export)")
        .with_examples(vec![
            ("Check a spec", "lex spec check sort.spec --source sort.lex"),
            ("Export SMT-LIB", "lex spec smt sort.spec"),
        ])
        .with_see_also(vec!["check", "conformance"]);
    info.subcommands = vec![check, smt];
    info
}

fn cmd_agent_tool() -> CommandInfo {
    CommandInfo::new("agent-tool", "have an LLM emit a Lex tool body, run it under declared effects")
        .idempotent(false)
        .add_option("--allow-effects", "string", "permitted effect kinds", None)
        .add_option("--request", "string", "natural-language request", None)
        .add_option("--body", "string", "inline tool body", None)
        .add_option("--body-file", "string", "tool body in a file", None)
        .add_option("--spec", "string", "spec file for behavioral verification", None)
        .add_option("--examples", "string", "JSON examples for behavioral checking", None)
        .add_option("--diff-body", "string", "differential evaluation: alternate body", None)
        .add_option("--diff-body-file", "string", "differential evaluation: alternate body file", None)
        .add_option("--json", "bool", "machine-readable output", None)
        .with_examples(vec![
            ("Run with allow-list", "lex agent-tool --allow-effects fs_read --request \"sum lines of /tmp/log\""),
            ("Verify against spec", "lex agent-tool --spec sort.spec --body-file body.lex --json"),
        ])
        .with_see_also(vec!["check", "spec", "run"])
}

fn cmd_tool_registry() -> CommandInfo {
    let serve = CommandInfo::new("serve", "HTTP service to register Lex tools and invoke them")
        .idempotent(false)
        .add_option("--port", "int", "TCP port", None);
    let mut info = CommandInfo::new("tool-registry", "runtime tool registration over HTTP")
        .with_examples(vec![
            ("Run on default port", "lex tool-registry serve"),
        ])
        .with_see_also(vec!["serve", "agent-tool"]);
    info.subcommands = vec![serve];
    info
}

fn cmd_audit() -> CommandInfo {
    CommandInfo::new("audit", "structural code search by effect / call / hostname / AST kind")
        .idempotent(true)
        .add_argument("paths", "string[]", "files / directories to scan", false)
        .add_option("--effect", "string", "effect kind filter", None)
        .add_option("--call", "string", "called-function filter", None)
        .add_option("--host", "string", "hostname filter (literal)", None)
        .add_option("--kind", "string", "AST node kind filter", None)
        .add_option("--json", "bool", "machine-readable output", None)
        .with_examples(vec![
            ("Find network calls", "lex audit --effect net examples"),
            ("Find a host as JSON", "lex audit --host api.example.com --json src"),
        ])
        .with_see_also(vec!["ast-diff", "ast-merge"])
}

fn cmd_ast_diff() -> CommandInfo {
    CommandInfo::new("ast-diff", "AST-native diff: added/removed/renamed/modified fns + body patches")
        .idempotent(true)
        .add_argument("file_a", "string", "left source file", true)
        .add_argument("file_b", "string", "right source file", true)
        .add_option("--json", "bool", "structured JSON output", None)
        .with_examples(vec![
            ("Compare two versions", "lex ast-diff a.lex b.lex"),
            ("Machine-readable", "lex ast-diff --json a.lex b.lex"),
        ])
        .with_see_also(vec!["audit", "ast-merge"])
}

fn cmd_ast_merge() -> CommandInfo {
    CommandInfo::new("ast-merge", "three-way structural merge with structured JSON conflicts")
        .idempotent(false)
        .add_argument("base", "string", "common ancestor source", true)
        .add_argument("ours", "string", "our side", true)
        .add_argument("theirs", "string", "their side", true)
        .add_option("--output", "string", "write merged source to a file", None)
        .add_option("--json", "bool", "emit conflicts as JSON", None)
        .with_examples(vec![
            ("Merge three versions", "lex ast-merge base.lex ours.lex theirs.lex"),
            ("Materialize merge", "lex ast-merge base.lex ours.lex theirs.lex --output merged.lex"),
        ])
        .with_see_also(vec!["ast-diff", "store-merge"])
}

fn cmd_branch() -> CommandInfo {
    let list = CommandInfo::new("list", "list branches in the store").idempotent(true)
        .add_option("--store", "string", "store root", None);
    let show = CommandInfo::new("show", "show a branch's head map").idempotent(true)
        .add_argument("name", "string", "branch name", true)
        .add_option("--store", "string", "store root", None);
    let create = CommandInfo::new("create", "create a branch").idempotent(false)
        .add_argument("name", "string", "branch name", true)
        .add_option("--from", "string", "parent branch (default: main)", None)
        .add_option("--store", "string", "store root", None);
    let delete = CommandInfo::new("delete", "delete a non-current, non-default branch")
        .idempotent(false)
        .add_argument("name", "string", "branch name", true)
        .add_option("--store", "string", "store root", None);
    let use_b = CommandInfo::new("use", "set the current branch").idempotent(false)
        .add_argument("name", "string", "branch name", true)
        .add_option("--store", "string", "store root", None);
    let current = CommandInfo::new("current", "print the current branch").idempotent(true)
        .add_option("--store", "string", "store root", None);
    let log = CommandInfo::new("log", "print the merge journal of a branch").idempotent(true)
        .add_argument("name", "string", "branch name (default: current)", false)
        .add_option("--store", "string", "store root", None);
    let mut info = CommandInfo::new("branch", "snapshot branches in lex-store (tier-1 agent-native VC)")
        .with_examples(vec![
            ("List branches", "lex branch list"),
            ("Create a feature", "lex branch create feature --from main"),
        ])
        .with_see_also(vec!["store-merge", "store", "log"]);
    info.subcommands = vec![list, show, create, delete, use_b, current, log];
    info
}

fn cmd_log() -> CommandInfo {
    CommandInfo::new("log", "show the merge journal of a branch (top-level alias for `lex branch log`)")
        .idempotent(true)
        .add_argument("name", "string", "branch name (default: current)", false)
        .add_option("--store", "string", "store root", None)
        .with_examples(vec![
            ("Log of the current branch", "lex log"),
            ("Log of a named branch as JSON", "lex --output json log feature"),
        ])
        .with_see_also(vec!["branch", "store-merge"])
}

fn cmd_store_merge() -> CommandInfo {
    CommandInfo::new("store-merge", "three-way merge between two branches in the store")
        .idempotent(false)
        .add_argument("src", "string", "source branch", true)
        .add_argument("dst", "string", "destination branch", true)
        .add_option("--commit", "bool", "apply a clean merge to dst (refused if conflicts)", None)
        .add_option("--json", "bool", "emit the merge report as JSON", None)
        .add_option("--store", "string", "store root directory", None)
        .with_examples(vec![
            ("Preview a merge", "lex store-merge feature main"),
            ("Commit a clean merge", "lex store-merge feature main --commit"),
        ])
        .with_see_also(vec!["branch", "ast-merge"])
}
