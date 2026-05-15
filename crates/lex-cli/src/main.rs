//! Lex CLI per spec §12.1.
//!
//! Usage:
//!   lex parse <file>
//!   lex check <file>
//!   lex run [--allow-effects k1,k2] [--allow-fs-read p] [--allow-fs-write p]
//!           [--budget N] <file> <fn> [<arg>...]
//!   lex hash <file>
//!   lex publish [--store DIR] [--activate] <file>
//!   lex store list [--store DIR]
//!   lex store get [--store DIR] <stage_id>

mod tool_registry;
mod audit;
mod diff;
mod ast_merge;
mod branch;
mod merge;
mod acli;
mod docs;
mod op;
mod repl;
mod lint;
mod pkg;
mod test_runner;
mod watch;
mod fmt;
mod init;
mod ci;
mod examples_eval;
mod agent_guidelines;

use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Context, Result};
use lex_ast::{canonicalize_program, stage_canonical_hash_hex, stage_id, Stage};
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_store::Store;
use lex_syntax::syntax::Program as SynProgram;
use lex_syntax::{load_program, load_program_from_str};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Pre-parse `--output` so we can route errors through ACLI's
    // error envelope when the agent asked for JSON. Errors here
    // (e.g. invalid format) fall back to text reporting since we
    // haven't yet committed to a format.
    let (fmt, rest_after_format) = match parse_output_format(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    };
    let cmd_for_err = rest_after_format.first().cloned()
        .unwrap_or_else(|| "lex".into());
    if let Err(e) = run(&fmt, &rest_after_format) {
        acli::emit_error(&cmd_for_err, &format!("{e:#}"), &fmt,
            ::acli::ExitCode::GeneralError);
        std::process::exit(1);
    }
}

fn run(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let cmd = args.first().ok_or_else(|| anyhow!("usage: lex <command> ..."))?;
    match cmd.as_str() {
        // ACLI built-ins — emit JSON envelopes via the SDK.
        "introspect" => { acli::build_app().handle_introspect(fmt); Ok(()) }
        "skill" => {
            let out_path = args.get(1).map(|s| s.as_str());
            acli::build_app().handle_skill(out_path, fmt);
            Ok(())
        }
        "version" | "--version" | "-V" => {
            acli::build_app().handle_version(fmt);
            Ok(())
        }
        "parse" => cmd_parse(fmt, &args[1..]),
        "check" => cmd_check(fmt, &args[1..]),
        "run" => cmd_run(fmt, &args[1..]),
        "hash" => cmd_hash(fmt, &args[1..]),
        "blame" => cmd_blame(fmt, &args[1..]),
        "publish" => cmd_publish(fmt, &args[1..]),
        "store" => cmd_store(fmt, &args[1..]),
        "stage" => cmd_stage(fmt, &args[1..]),
        "attest" => cmd_attest(fmt, &args[1..]),
        "trace" => cmd_trace(fmt, &args[1..]),
        "replay" => cmd_replay(fmt, &args[1..]),
        "diff" => cmd_diff(fmt, &args[1..]),
        "serve" => cmd_serve(&args[1..]),
        "conformance" => cmd_conformance(fmt, &args[1..]),
        "spec" => cmd_spec(fmt, &args[1..]),
        "agent-tool" => {
            // agent-tool has its own `--json`; propagate `--output json`
            // into it without breaking the legacy flag.
            let mut a: Vec<String> = args[1..].to_vec();
            if matches!(fmt, OutputFormat::Json) && !a.iter().any(|s| s == "--json") {
                a.push("--json".into());
            }
            cmd_agent_tool(&a)
        }
        "tool-registry" => tool_registry::cmd_tool_registry(&args[1..]),
        "audit" => audit::cmd_audit(fmt, &args[1..]),
        "ast-diff" => diff::cmd_diff(fmt, &args[1..]),
        "ast-merge" => ast_merge::cmd_ast_merge(fmt, &args[1..]),
        "branch" => branch::cmd_branch(fmt, &args[1..]),
        "store-merge" => branch::cmd_store_merge(fmt, &args[1..]),
        "merge" => merge::cmd_merge(fmt, &args[1..]),
        "policy" => cmd_policy(fmt, &args[1..]),
        "log" => branch::cmd_log(fmt, &args[1..]),
        "op" => op::cmd_op(fmt, &args[1..]),
        "docs" => docs::cmd_docs(fmt, &args[1..]),
        "plan" => cmd_plan(fmt, &args[1..]),
        "repair" => cmd_repair(fmt, &args[1..]),
        "producer-trust" => cmd_producer_trust(fmt, &args[1..]),
        "canonical" => cmd_canonical(fmt, &args[1..]),
        "keygen" => cmd_keygen(fmt, &args[1..]),
        "pkg"  => pkg::cmd_pkg(&args[1..]),
        "repl" => repl::cmd_repl(&args[1..]),
        "test" => test_runner::cmd_test(fmt, &args[1..]),
        "watch" => watch::cmd_watch(&args[1..]),
        "fmt"  => fmt::cmd_fmt(&args[1..]),
        "init" => init::cmd_init(&args[1..]),
        "ci"   => ci::cmd_ci(&args[1..]),
        "agent-guidelines" => agent_guidelines::cmd_agent_guidelines(&fmt, &args[1..]),
        "help" | "--help" | "-h" => { print_usage(); Ok(()) }
        other => bail!("unknown command `{other}`. try `lex help`"),
    }
}

/// Strip a leading `--output FORMAT` (or `--output=FORMAT`) from
/// `args`. Accepts `text` / `json` / `table`. Defaults to text.
/// We only scan up to the first non-`--output` token so we don't
/// swallow per-subcommand `--output` flags (e.g. `lex ast-merge
/// --output merged.lex`, which is a path, not a format).
fn parse_output_format(args: &[String]) -> Result<(OutputFormat, Vec<String>)> {
    use std::str::FromStr;
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    let mut format = OutputFormat::Text;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--output" && i + 1 < args.len() {
            format = OutputFormat::from_str(&args[i + 1]).map_err(|e| anyhow!(e))?;
            i += 2;
        } else if let Some(v) = a.strip_prefix("--output=") {
            format = OutputFormat::from_str(v).map_err(|e| anyhow!(e))?;
            i += 1;
        } else {
            // Stop scanning at first positional / unrelated flag — the
            // remaining `--output` (if any) belongs to a subcommand.
            out.extend_from_slice(&args[i..]);
            break;
        }
    }
    Ok((format, out))
}

fn print_usage() {
    println!("lex — Lex toolchain\n");
    println!("commands:");
    println!("  init [<dir>]                       scaffold a new project (lex.toml, src/, tests/, CI)");
    println!("  parse <file>                       print canonical AST as JSON");
    println!("  check [--strict] <file>            type-check; --strict adds lint warnings");
    println!("  fmt [--check] <file|dir>...        format .lex files; --check exits 1 if any need it");
    println!("  ci [--no-fmt] [--src <d>] [--tests <d>]");
    println!("                                     run the full pipeline: pkg install, check --strict,");
    println!("                                     fmt --check, test — same as CI in lex.yml");
    println!("  pkg init                           create lex.toml in current directory");
    println!("  pkg add <name> --path <p>          add a local path dependency");
    println!("  pkg add <name> --git <url>         add a git dependency");
    println!("  pkg install                        install/verify all declared dependencies");
    println!("  pkg list                           list declared dependencies");
    println!("  run [policy] <file> <fn> [args]    execute fn (args parsed as JSON)");
    println!("  run --from-store STAGE_ID [--require-signed] [--trusted-key HEX] <fn> [args]");
    println!("                                     run a stage straight out of the store;");
    println!("                                     verify Ed25519 signature when present.");
    println!("  hash <file>                        print stage canonical hashes");
    println!("  publish [--store DIR] [--branch NAME] [--activate] [--signing-key HEX] <file>");
    println!("                                     publish each stage to the store as Draft;");
    println!("                                     --signing-key (or LEX_SIGNING_KEY) attaches an");
    println!("                                     Ed25519 signature over each StageId.");
    println!("  keygen                             print a fresh Ed25519 keypair (hex)");
    println!("  store list [--store DIR]           list SigIds in the store");
    println!("  store get [--store DIR] [--require-signed] [--trusted-key HEX] <stage>");
    println!("                                     print stage metadata + canonical AST;");
    println!("                                     verify Ed25519 signature when present.");
    println!("  store search [--store DIR] [--limit N] \"<query>\"");
    println!("  store migrate-ops [--store DIR] --to v1 [--dry-run | --confirm]");
    println!("                                     semantic search over active stages,");
    println!("                                     ranked by description+signature+examples.");
    println!("  stage <stage> [--attestations]     print stage info, or list its attestations");
    println!("  attest filter [--kind K] [--result R] [--since T] [--store DIR]");
    println!("  attest retro-block --producer TOOL_ID --reason \"...\" [--store DIR]");
    println!("  attest retro-unblock --producer TOOL_ID --reason \"...\" [--store DIR]");
    println!("                                     cross-stage attestation queries");
    println!("  trace <run_id>                     print a saved trace tree as JSON");
    println!("  replay <run_id> <file> <fn> [args] [--override NODE=JSON]...");
    println!("                                     re-execute with effect overrides keyed by NodeId");
    println!("  diff <run_a> <run_b>               first NodeId where two traces diverge");
    println!("  serve [--port N] [--store DIR]     start the agent API HTTP server");
    println!("  repl [--load <file>]...            interactive evaluator; --load pre-loads source");
    println!("  test [<dir>]                       run tests/test_*.lex files (calls run_all in each)");
    println!("  conformance <dir>                  run all JSON test descriptors in <dir>");
    println!("  spec check <spec> --source <file> [--store DIR] [--trials N]");
    println!("                                     check a Spec against a Lex source");
    println!("                                     (--store: persist a Spec attestation)");
    println!("  spec smt <spec>                    emit SMT-LIB for external Z3");
    println!("  agent-tool [--allow-effects ks] (--request 'q' | --body-file F | --body 'B')");
    println!("                                     have an LLM emit a Lex tool body, run it");
    println!("                                     under the declared effects (rejected at");
    println!("                                     type-check if it tries anything else)");
    println!("  tool-registry serve [--port N]    HTTP service to register Lex tools at runtime");
    println!("                                     and invoke them via /tools/{{id}}/invoke");
    println!("  audit [paths...] [filters]        structural code search by effect / call /");
    println!("                                     hostname / AST kind. --json for machine-readable.");
    println!("  audit --query \"<text>\" [--limit N] [--effect K]");
    println!("                                     semantic search over the store; --effect");
    println!("                                     post-filters the ranked list.");
    println!("  ast-diff <file_a> <file_b>        AST-native diff: added/removed/renamed/modified");
    println!("                                     fns, plus body-level patches per modified body.");
    println!("  ast-merge <base> <ours> <theirs>  three-way structural merge; structured-JSON");
    println!("                                     conflicts via --json; --output writes merged source.");
    println!("  branch <subcommand> ...           snapshot branches in lex-store. subcommands:");
    println!("                                     list | show <name> | create <name> [--from B] |");
    println!("                                     delete <name> | use <name> | current");
    println!("  store-merge <src> <dst> [--commit] [--json]  three-way merge between two branches in");
    println!("                                     the store; conflicts as JSON. --commit applies a");
    println!("                                     clean merge; refuses if any conflicts remain.");
    println!("  merge {{start|status|resolve|defer|commit}}");
    println!("                                     stateful merge for agent loops (#134); persists");
    println!("                                     a session under <store>/merges/<merge_id>.json");
    println!("  policy {{block-producer|unblock-producer|require-attestation|");
    println!("          unrequire-attestation|show}}");
    println!("                                     manage <store>/policy.json — negative gate on");
    println!("                                     producers (#181) and positive gate on required");
    println!("                                     attestations for branch advance (#245)");
    println!();
    println!("policy flags (run, replay):");
    println!("  --allow-effects k1,k2,...   permit these effect kinds");
    println!("  --allow-fs-read PATH        (repeatable) permit fs_read under PATH");
    println!("  --allow-fs-write PATH       (repeatable) permit fs_write under PATH");
    println!("  --budget N                  cap aggregate declared budget");
    println!("  --max-steps N               cap VM opcode dispatches at N (DoS guard)");
}

fn read_source(path: &str) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("reading stdin")?;
        Ok(s)
    } else {
        fs::read_to_string(path).with_context(|| format!("reading {path}"))
    }
}

/// Read a Lex program from a file path or `-` (stdin), expanding local
/// imports relative to the file's directory. For stdin, local imports
/// are rejected (no base path to resolve from).
fn read_program(path: &str) -> Result<SynProgram> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s).context("reading stdin")?;
        load_program_from_str(&s).map_err(Into::into)
    } else {
        load_program(std::path::Path::new(path)).map_err(Into::into)
    }
}

/// Load a program as canonical AST stages, choosing between the
/// text parser and the canonical-AST decoder by `from_canonical`
/// (#206 slice 3). Both paths produce the same `Vec<Stage>` shape;
/// the difference is whether the parse step runs at all. Agents
/// that build canonical AST directly avoid parser-bug blast radius
/// and skip a CPU-bound step, which is part of the slice-1
/// motivation.
fn load_stages(path: &str, from_canonical: bool) -> Result<Vec<lex_ast::Stage>> {
    if from_canonical {
        let bytes = if path == "-" {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf).context("reading stdin")?;
            buf
        } else {
            std::fs::read(path).map_err(|e| anyhow!("read {path}: {e}"))?
        };
        lex_ast::canonical_format::decode_program(&bytes)
            .map_err(|e| anyhow!("decode {path}: {e}"))
    } else {
        let prog = read_program(path)?;
        Ok(canonicalize_program(&prog))
    }
}

/// Load a single stage out of the default `lex-store` and verify
/// its signature against the supplied policy. The returned program
/// is `vec![stage]` — the function being called is expected to be
/// self-contained inside that stage. Imports / cross-stage refs
/// would need a richer load path; this slice keeps the surface
/// minimal.
fn load_stages_from_store(
    stage_id: &str,
    require_signed: bool,
    trusted_key: Option<&str>,
) -> Result<Vec<lex_ast::Stage>> {
    let store = lex_store::Store::open(default_store_root())
        .with_context(|| "opening default store")?;
    let meta = store.get_metadata(stage_id)
        .with_context(|| format!("loading metadata for stage `{stage_id}`"))?;
    verify_metadata_signature(&meta, require_signed, trusted_key)?;
    let stage = store.get_ast(stage_id)
        .with_context(|| format!("loading AST for stage `{stage_id}`"))?;
    Ok(vec![stage])
}

fn cmd_parse(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!("usage: lex parse <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let data = serde_json::to_value(&stages)?;
    acli::emit_or_text("parse", data.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    });
    Ok(())
}

fn cmd_check(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // #206 slice 3: `--from-canonical` reads the program as
    // canonical-AST bytes instead of `.lex` text.
    let mut from_canonical = false;
    let mut strict = false;
    let mut path: Option<&str> = None;
    for a in args {
        match a.as_str() {
            "--from-canonical" => { from_canonical = true; }
            "--strict" => { strict = true; }
            other if !other.starts_with("--") => {
                if path.is_some() {
                    bail!("usage: lex check [--from-canonical] [--strict] <file>");
                }
                path = Some(other);
            }
            other => bail!("unknown flag `{other}` for `lex check`"),
        }
    }
    let path = path.ok_or_else(|| anyhow!(
        "usage: lex check [--from-canonical] [--strict] <file>"))?;
    let stages = load_stages(path, from_canonical)?;

    // #306 slice 1: when checking a `.lex` source file (not a
    // pre-built canonical AST), collect each `fn` declaration's
    // source position so type errors can be reported as
    // `file:line:col` instead of bare NodeIds. Skipped under
    // `--from-canonical` since canonical bytes carry no source span.
    let positions: Option<std::collections::BTreeMap<String, lex_types::Position>> =
        if !from_canonical && path != "-" {
            std::fs::read_to_string(path).ok().and_then(|src| {
                lex_syntax::parse_source_with_positions(&src).ok().map(|(_, fn_pos)| {
                    fn_pos.into_iter().map(|(name, byte)| {
                        let (line, col) = lex_types::byte_to_line_col(&src, byte);
                        (name, lex_types::Position::new(Some(path.to_string()), line, col))
                    }).collect()
                })
            })
        } else {
            None
        };

    let check_result = match &positions {
        Some(pos) => lex_types::check_program_with_positions(&stages, pos)
            .map_err(|errs| errs.into_iter().collect::<Vec<_>>()),
        None => lex_types::check_program(&stages)
            .map_err(|errs| errs.into_iter().map(lex_types::PositionedError::from).collect()),
    };

    match check_result {
        Ok(_) => {
            // #369 slice 2: behavioral evaluation of `examples { ... }` blocks.
            // Type-level checks ran inside `check_program`; now we actually
            // run each example case through the VM and compare to the
            // declared expected value. Any mismatches surface through the
            // same JSON envelope as type errors and exit 2 — they're hard
            // errors, not lints, because the `examples` block is meant to
            // be load-bearing contract, not a warning.
            let example_errors = examples_eval::evaluate_examples(&stages);
            if !example_errors.is_empty() {
                let positioned: Vec<lex_types::PositionedError> = example_errors
                    .into_iter()
                    .map(lex_types::PositionedError::from)
                    .collect();
                let arr: Vec<serde_json::Value> = positioned
                    .iter()
                    .map(|e| serde_json::to_value(e).unwrap())
                    .collect();
                let data = serde_json::json!({ "ok": false, "errors": arr });
                acli::emit_or_text("check", data, fmt, || {
                    for e in &positioned {
                        if let Ok(j) = serde_json::to_string(e) {
                            println!("{j}");
                        }
                    }
                });
                std::process::exit(2);
            }

            // --strict: run AST lint passes + bytecode stack verifier (#347 A2).
            // Warnings are non-fatal but exit 1 so CI can enforce them.
            let mut lint_warnings = if strict && !from_canonical && path != "-" {
                std::fs::read_to_string(path).ok()
                    .and_then(|src| lex_syntax::parse_source(&src).ok())
                    .map(|prog| lint::lint_program(&prog))
                    .unwrap_or_default()
            } else {
                vec![]
            };

            // Third --strict check (#347 A2): bytecode stack-depth verifier.
            // Compiles the type-checked program and verifies that every branch
            // merge point has a consistent stack depth — catching PConstructor
            // stack leaks that the type checker cannot see.
            if strict {
                let bytecode = compile_program(&stages);
                for err in lex_bytecode::verify_program(&bytecode.functions) {
                    lint_warnings.push(lint::LintWarning {
                        code: "STACK_DEPTH",
                        message: format!(
                            "stack depth mismatch at pc {} in `{}`: \
                             path A depth {}, path B depth {} — \
                             a match arm may have leaked or over-consumed stack values",
                            err.pc, err.fn_name, err.depth_a, err.depth_b
                        ),
                        location: format!("fn `{}`", err.fn_name),
                    });
                }
            }

            let summary = effects_summary(&stages);
            let data = serde_json::json!({
                "ok": lint_warnings.is_empty(),
                "stages": stages.len(),
                "required_effects": summary.kinds,
                "required_fs_read": summary.fs_read,
                "required_fs_write": summary.fs_write,
                "required_net_host": summary.net_host,
                "warnings": lint_warnings,
            });
            acli::emit_or_text("check", data, fmt, || {
                if lint_warnings.is_empty() {
                    println!("ok");
                } else {
                    for w in &lint_warnings {
                        println!("[{}] {} ({})", w.code, w.message, w.location);
                    }
                }
                if !summary.kinds.is_empty() {
                    println!("required effects: {}", summary.kinds.join(", "));
                    if !summary.fs_read.is_empty() {
                        println!("required fs_read paths: {}", summary.fs_read.join(", "));
                    }
                    if !summary.fs_write.is_empty() {
                        println!("required fs_write paths: {}", summary.fs_write.join(", "));
                    }
                    if !summary.net_host.is_empty() {
                        println!("required net hosts: {}", summary.net_host.join(", "));
                    }
                    println!("hint: lex run {} {path} <fn> [args]", suggest_grants(&summary));
                }
            });
            if !lint_warnings.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(errs) => {
            let arr: Vec<serde_json::Value> = errs.iter()
                .map(|e| serde_json::to_value(e).unwrap()).collect();
            let data = serde_json::json!({ "ok": false, "errors": arr });
            acli::emit_or_text("check", data, fmt, || {
                for e in &errs {
                    if let Ok(j) = serde_json::to_string(e) {
                        println!("{j}");
                    }
                }
            });
            std::process::exit(2);
        }
    }
}

/// Effects required by a program, broken out by kind so the user can
/// see which `--allow-*` flags they'll need at run time. We aggregate
/// across every fn declaration in the program: more permissive than
/// strictly necessary (a single fn might need fewer effects), but
/// matches the common case of "I just want to run main".
struct EffectsSummary {
    kinds: Vec<String>,
    fs_read: Vec<String>,
    fs_write: Vec<String>,
    net_host: Vec<String>,
}

fn effects_summary(stages: &[lex_ast::Stage]) -> EffectsSummary {
    use std::collections::BTreeSet;
    let mut kinds: BTreeSet<String> = BTreeSet::new();
    let mut fs_read: BTreeSet<String> = BTreeSet::new();
    let mut fs_write: BTreeSet<String> = BTreeSet::new();
    let mut net_host: BTreeSet<String> = BTreeSet::new();
    for s in stages {
        if let lex_ast::Stage::FnDecl(fd) = s {
            for e in &fd.effects {
                kinds.insert(e.name.clone());
                if let Some(arg) = &e.arg {
                    let arg_str = match arg {
                        lex_ast::EffectArg::Str { value } => value.clone(),
                        lex_ast::EffectArg::Int { value } => value.to_string(),
                        lex_ast::EffectArg::Ident { value } => value.clone(),
                    };
                    match e.name.as_str() {
                        "fs_read" => { fs_read.insert(arg_str); }
                        "fs_write" => { fs_write.insert(arg_str); }
                        "net" => { net_host.insert(arg_str); }
                        _ => {}
                    }
                }
            }
        }
    }
    EffectsSummary {
        kinds: kinds.into_iter().collect(),
        fs_read: fs_read.into_iter().collect(),
        fs_write: fs_write.into_iter().collect(),
        net_host: net_host.into_iter().collect(),
    }
}

fn suggest_grants(s: &EffectsSummary) -> String {
    let mut parts = vec![format!("--allow-effects {}", s.kinds.join(","))];
    for p in &s.fs_read {
        parts.push(format!("--allow-fs-read {p}"));
    }
    for p in &s.fs_write {
        parts.push(format!("--allow-fs-write {p}"));
    }
    for h in &s.net_host {
        parts.push(format!("--allow-net-host {h}"));
    }
    parts.join(" ")
}

fn cmd_run(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let f = parse_run_flags(args)?;
    // #227 follow-up: when `--from-store` is set, the first
    // positional is the function name (no path needed). Otherwise
    // the legacy shape `lex run <file> <fn> [args]` applies.
    let (source_label, func, arg_positional_start) = if f.from_store.is_some() {
        let func = f.positional.first().ok_or_else(|| anyhow!(
            "usage: lex run --from-store STAGE_ID [--require-signed] [--trusted-key HEX] <fn> [args]"))?;
        (
            format!("store:{}", f.from_store.as_deref().unwrap()),
            func.clone(),
            1,
        )
    } else {
        let path = f.positional.first().ok_or_else(|| anyhow!(
            "usage: lex run [policy] [--from-canonical] <file> <fn> [args]"))?;
        let func = f.positional.get(1).ok_or_else(|| anyhow!("missing function name"))?;
        (path.clone(), func.clone(), 2)
    };
    let policy = &f.policy;
    if f.dry_run {
        let actions = vec![serde_json::json!({
            "action": "execute",
            "source": &source_label,
            "function": func,
            "args": &f.positional[arg_positional_start..],
            "policy": {
                "allow_effects": policy.allow_effects.iter().collect::<Vec<_>>(),
                "allow_fs_read": policy.allow_fs_read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "allow_fs_write": policy.allow_fs_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "allow_net_host": &policy.allow_net_host,
                "budget": policy.budget,
            },
            "trace": f.trace,
            "max_steps": f.max_steps,
        })];
        acli::emit_dry_run("run", fmt,
            &format!("would call `{func}` in {source_label}"), actions);
    }
    // #206 slice 3 (text/canonical paths) or #227 follow-up
    // (store path). Each produces the same Vec<Stage>; the typecheck
    // and compile pipeline is identical from this point on.
    let mut stages = if let Some(stage_id) = &f.from_store {
        load_stages_from_store(stage_id, f.require_signed, f.trusted_key.as_deref())?
    } else {
        load_stages(&source_label, f.from_canonical)?
    };
    // #168: rewrite stdlib parse calls during type-check so the
    // runtime sees the strict (validated) shape.
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
        let arr: Vec<serde_json::Value> = errs.iter()
            .map(|e| serde_json::to_value(e).unwrap()).collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("run", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) { eprintln!("{j}"); }
            }
        });
        std::process::exit(2);
    }
    let bc = compile_program(&stages);

    if let Err(violations) = check_policy(&bc, policy) {
        let arr: Vec<serde_json::Value> = violations.iter()
            .map(|v| serde_json::to_value(v).unwrap()).collect();
        let data = serde_json::json!({ "phase": "policy", "violations": arr });
        acli::emit_or_text("run", data, fmt, || {
            for v in &violations {
                if let Ok(j) = serde_json::to_string(v) { eprintln!("{j}"); }
            }
        });
        std::process::exit(3);
    }

    let bc = std::sync::Arc::new(bc);
    let handler = DefaultHandler::new(f.policy.clone()).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    if let Some(n) = f.max_steps { vm.set_step_limit(n); }
    let recorder = lex_trace::Recorder::new();
    let trace_handle = recorder.handle();
    if f.trace { vm.set_tracer(Box::new(recorder)); }
    // #257: snapshot the default branch's head before the run so we
    // can attribute any ops committed during the run back to the
    // run's `run_id` via Trace attestations. `pre_run_head` is
    // `None` for a fresh store; that's fine — `record_run_committed_ops_since`
    // treats `None` as "every reachable op is post-run", which is
    // the right behavior on an empty pre-run history.
    let pre_run_head = if f.trace {
        let store = lex_store::Store::open(default_store_root())?;
        store.get_branch(lex_store::DEFAULT_BRANCH)
            .map_err(|e| anyhow!("reading branch: {e}"))?
            .and_then(|b| b.head_op)
    } else {
        None
    };

    let vargs: Vec<Value> = f.positional[arg_positional_start..]
        .iter()
        .map(|a| {
            let v: serde_json::Value = serde_json::from_str(a)
                .with_context(|| format!("arg `{a}` must be JSON"))?;
            Ok(json_to_value(&v))
        })
        .collect::<Result<Vec<_>>>()?;
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let result = vm.call(&func, vargs);
    let ended = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let mut trace_id: Option<String> = None;
    if f.trace {
        let store = lex_store::Store::open(default_store_root())?;
        let (root_out, root_err) = match &result {
            Ok(v) => (Some(value_to_json(v)), None),
            Err(e) => (None, Some(format!("{e}"))),
        };
        let tree = trace_handle.finalize(func.clone(), serde_json::Value::Null,
            root_out, root_err, started, ended);
        let id = store.save_trace(&tree)?;
        // #246: emit a `Trace` attestation linking the run to the
        // entry stage. Skipped silently if the entry function isn't
        // resolvable to a stage in the loaded program — `lex run`
        // accepts plain `.lex` files that may carry sigs not yet
        // published to the store, and the audit hook is informational
        // rather than load-bearing.
        if let Some(entry_stage_id) = entry_stage_id_for(&stages, &func) {
            let attestation = lex_vcs::Attestation::new(
                entry_stage_id,
                None,
                None,
                lex_vcs::AttestationKind::Trace {
                    run_id: id.clone(),
                    root_target: func.clone(),
                },
                match &result {
                    Ok(_) => lex_vcs::AttestationResult::Passed,
                    Err(e) => lex_vcs::AttestationResult::Failed {
                        detail: format!("{e}"),
                    },
                },
                trace_producer(),
                None,
            );
            // Use the store's attestation log helper so the file
            // layout is consistent with `lex publish`'s emissions.
            store.attestation_log()
                .map_err(|e| anyhow!("opening attestation log: {e}"))?
                .put(&attestation)
                .map_err(|e| anyhow!("recording trace attestation: {e}"))?;
        }
        // #257: emit `Trace` attestations with `op_id` set for any
        // op committed during the run. Walks `ops_since(post_head,
        // pre_run_head)` on the default branch — the only branch
        // `lex run` interacts with today. Empty for the common case
        // where the program doesn't commit ops.
        let att_result = match &result {
            Ok(_) => lex_vcs::AttestationResult::Passed,
            Err(e) => lex_vcs::AttestationResult::Failed {
                detail: format!("{e}"),
            },
        };
        let n_op_traces = store.record_run_committed_ops_since(
            &id,
            &func,
            lex_store::DEFAULT_BRANCH,
            pre_run_head.as_ref(),
            att_result,
            trace_producer(),
        ).map_err(|e| anyhow!("recording op traces: {e}"))?;
        if n_op_traces > 0 && !matches!(fmt, OutputFormat::Json) {
            eprintln!("trace attestations: {n_op_traces} op(s) linked to run");
        }
        trace_id = Some(id.clone());
        if !matches!(fmt, OutputFormat::Json) { eprintln!("trace saved: {id}"); }
    }
    let r = result.map_err(|e| anyhow!("runtime: {e}"))?;
    let result_json = value_to_json(&r);
    let data = match &trace_id {
        Some(id) => serde_json::json!({ "result": result_json, "trace_id": id }),
        None => serde_json::json!({ "result": result_json }),
    };
    acli::emit_or_text("run", data, fmt, || println!("{}", value_to_json_string(&r)));
    Ok(())
}

/// Parsed arguments for `lex run`.
#[derive(Default)]
struct RunFlags {
    policy: Policy,
    positional: Vec<String>,
    trace: bool,
    max_steps: Option<u64>,
    dry_run: bool,
    from_canonical: bool,
    /// `--from-store STAGE_ID` (#227 follow-up). Loads the stage's
    /// canonical AST out of the store instead of reading a file. The
    /// fn-arg must name a function that exists in the loaded stage.
    from_store: Option<String>,
    /// Refuse to run an unsigned stage (only meaningful with
    /// `--from-store`). Implied by `--trusted-key`.
    require_signed: bool,
    /// Hex Ed25519 public key the stage must be signed by.
    trusted_key: Option<String>,
}

fn parse_run_flags(args: &[String]) -> Result<RunFlags> {
    let mut f = RunFlags { policy: Policy::pure(), ..Default::default() };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--allow-effects" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                f.policy.allow_effects = val.split(',').filter(|s| !s.is_empty())
                    .map(|s| s.to_string()).collect::<BTreeSet<_>>();
                i += 2;
            }
            "--allow-fs-read" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-fs-read needs a value"))?;
                f.policy.allow_fs_read.push(PathBuf::from(val));
                i += 2;
            }
            "--allow-fs-write" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-fs-write needs a value"))?;
                f.policy.allow_fs_write.push(PathBuf::from(val));
                i += 2;
            }
            "--allow-net-host" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-net-host needs a value"))?;
                f.policy.allow_net_host.push(val.clone());
                i += 2;
            }
            "--allow-proc" => {
                // Comma-separated binary basenames the [proc] effect
                // is allowed to spawn. Read SECURITY.md before granting.
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-proc needs a value"))?;
                for name in val.split(',').filter(|s| !s.is_empty()) {
                    f.policy.allow_proc.push(name.to_string());
                }
                i += 2;
            }
            "--budget" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--budget needs a value"))?;
                f.policy.budget = Some(val.parse().context("--budget must be an integer")?);
                i += 2;
            }
            "--max-steps" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--max-steps needs a value"))?;
                f.max_steps = Some(val.parse().context("--max-steps must be an integer")?);
                i += 2;
            }
            "--trace" => { f.trace = true; i += 1; }
            "--dry-run" => { f.dry_run = true; i += 1; }
            "--from-canonical" => {
                // #206 slice 3: read the program as canonical-AST
                // bytes instead of `.lex` text. The path argument
                // points to the bytes file (or `-` for stdin); the
                // text parser is bypassed entirely on this path.
                f.from_canonical = true;
                i += 1;
            }
            "--from-store" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--from-store needs a stage_id"))?;
                f.from_store = Some(val.clone());
                i += 2;
            }
            "--require-signed" => { f.require_signed = true; i += 1; }
            "--trusted-key" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--trusted-key needs a hex value"))?;
                f.trusted_key = Some(val.clone());
                f.require_signed = true;
                i += 2;
            }
            _ => { f.positional.push(a.clone()); i += 1; }
        }
    }
    Ok(f)
}

/// `lex trace <run_id>` — load the trace tree by run id (existing).
/// `lex trace --op <op_id>` (#246) — list every `AttestationKind::Trace`
/// attestation whose `op_id` field matches. Populated by the
/// ops-during-run pipeline (#257): when `lex run --trace` finds
/// any op committed during the run, it emits per-stage Trace
/// attestations with `op_id: Some(...)` set, which this filter
/// surfaces. The entry-point Trace attestation (no op_id) is not
/// returned — it's not associated with a single committed op.
fn cmd_trace(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // --op flag form first.
    let mut op_filter: Option<String> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--op" => {
                op_filter = Some(args.get(i + 1)
                    .ok_or_else(|| anyhow!("--op needs an op_id"))?
                    .clone());
                i += 2;
            }
            "--store" => {
                store_root = Some(PathBuf::from(args.get(i + 1)
                    .ok_or_else(|| anyhow!("--store needs a path"))?));
                i += 2;
            }
            other if !other.starts_with("--") => {
                positional.push(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let root = store_root.unwrap_or_else(default_store_root);
    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;

    if let Some(op_id) = op_filter {
        let log = store.attestation_log()?;
        let traces: Vec<lex_vcs::Attestation> = log.list_all()?
            .into_iter()
            .filter(|a| matches!(a.kind, lex_vcs::AttestationKind::Trace { .. })
                && a.op_id.as_deref() == Some(op_id.as_str()))
            .collect();
        let data = serde_json::json!({
            "op_id": op_id,
            "count": traces.len(),
            "traces": serde_json::to_value(&traces)?,
        });
        let listing = traces.clone();
        acli::emit_or_text("trace", data, fmt, move || {
            if listing.is_empty() {
                println!("(no Trace attestations for op {op_id})");
                return;
            }
            for a in &listing {
                if let lex_vcs::AttestationKind::Trace { run_id, root_target } = &a.kind {
                    println!("{run_id}\t{root_target}\tat={}", a.timestamp);
                }
            }
        });
        return Ok(());
    }

    // Positional path: load and dump the trace tree.
    let id = positional.first().ok_or_else(|| anyhow!(
        "usage: lex trace <run_id> | lex trace --op <op_id> [--store DIR]"
    ))?;
    let tree = store.load_trace(id)?;
    let data = serde_json::to_value(&tree)?;
    acli::emit_or_text("trace", data.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    });
    Ok(())
}

/// Find the `stage_id` of the entry-point function in a parsed
/// program. Used by `lex run --trace` (#246) to attach a stage
/// reference to the emitted [`AttestationKind::Trace`]. Returns
/// `None` when the function name doesn't match any FnDecl in the
/// program — typically because the caller passed a stdlib name or
/// a function the file doesn't actually define.
fn entry_stage_id_for(stages: &[lex_ast::Stage], func: &str) -> Option<String> {
    for stage in stages {
        if let lex_ast::Stage::FnDecl(fd) = stage {
            if fd.name == func {
                return lex_ast::stage_id(stage);
            }
        }
    }
    None
}

/// Producer for the `Trace` attestation emitted by `lex run --trace`
/// (#246). Tagged as `lex-cli` (not `lex-store`) because the run
/// command — not the store gate — is what notices a tracer was
/// active.
fn trace_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex run --trace".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

fn cmd_diff(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let a = args.first().ok_or_else(|| anyhow!("usage: lex diff <run_a> <run_b>"))?;
    let b = args.get(1).ok_or_else(|| anyhow!("missing second run id"))?;
    let store = lex_store::Store::open(default_store_root())?;
    let ta = store.load_trace(a)?;
    let tb = store.load_trace(b)?;
    let data = match lex_trace::diff_runs(&ta, &tb) {
        Some(d) => serde_json::to_value(&d)?,
        None => serde_json::json!({ "divergence": null }),
    };
    acli::emit_or_text("diff", data.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    });
    Ok(())
}

/// `lex canonical <encode|decode>` dispatcher (#206 slice 2).
fn cmd_canonical(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex canonical <encode|decode> ..."))?;
    match sub.as_str() {
        "encode" => cmd_canonical_encode(fmt, &args[1..]),
        "decode" => cmd_canonical_decode(fmt, &args[1..]),
        other => bail!("unknown `lex canonical` action `{other}`; \
                       expected `encode` or `decode`"),
    }
}

/// `lex canonical encode <text-file> [--out <bytes-file>]` (#206 slice 2).
///
/// Parses a `.lex` source file, canonicalizes it, and emits the
/// versioned canonical-AST byte representation. Without `--out`,
/// writes raw bytes to stdout (suitable for piping into another
/// agent process or `lex canonical decode`); with `--out`, writes
/// to the named file.
///
/// JSON-output mode (`--output json`) emits a structured envelope
/// instead — `{ "ok": true, "bytes_b64": "..." }` — so agent
/// harnesses can capture the canonical bytes without dealing with
/// raw-bytes-on-stdout encoding issues.
fn cmd_canonical_encode(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut path: Option<&str> = None;
    let mut out_path: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                out_path = Some(args.get(i + 1).map(|s| s.as_str())
                    .ok_or_else(|| anyhow!("--out needs a path"))?);
                i += 2;
            }
            s if s.starts_with("--") => bail!("unknown flag `{s}` for `lex canonical encode`"),
            _ => {
                if path.is_some() {
                    bail!("usage: lex canonical encode <text-file> [--out <bytes-file>]");
                }
                path = Some(args[i].as_str());
                i += 1;
            }
        }
    }
    let path = path.ok_or_else(|| anyhow!(
        "usage: lex canonical encode <text-file> [--out <bytes-file>]"))?;

    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let bytes = lex_ast::canonical_format::encode_program(&stages);

    if let Some(out) = out_path {
        std::fs::write(out, &bytes)
            .map_err(|e| anyhow!("write {out}: {e}"))?;
        let data = serde_json::json!({
            "ok": true,
            "out": out,
            "bytes": bytes.len(),
            "stages": stages.len(),
        });
        acli::emit_or_text("canonical-encode", data, fmt, || {
            println!("wrote {} bytes to {out}", bytes.len());
        });
    } else {
        match fmt {
            OutputFormat::Json => {
                let b64 = encode_b64(&bytes);
                let data = serde_json::json!({
                    "ok": true,
                    "bytes_b64": b64,
                    "stages": stages.len(),
                });
                acli::emit_or_text("canonical-encode", data, fmt, || {});
            }
            _ => {
                use std::io::Write;
                std::io::stdout().write_all(&bytes)
                    .map_err(|e| anyhow!("stdout: {e}"))?;
            }
        }
    }
    Ok(())
}

// ---- #227: ed25519 keygen + signing helpers ----------------------

/// `lex keygen` — print a fresh Ed25519 keypair as hex.
///
/// Default text output is two lines:
///
///   `public_key  <hex>`
///   `secret_key  <hex>`
///
/// JSON output emits `{ "public_key": "...", "secret_key": "..." }`.
/// The secret key is printed once and never persisted by Lex itself —
/// the caller is responsible for storing it (env var, secret manager,
/// hardware token, etc.).
/// Build the embedder used by `lex store search` / `lex audit
/// --query`. When `LEX_EMBED_URL` is set we wire up an HTTP backend
/// (Ollama or OpenAI-compat per `LEX_EMBED_PROVIDER`) wrapped in a
/// filesystem cache under `<store>/search/embeddings/`. Otherwise
/// we use the deterministic [`lex_search::MockEmbedder`].
pub(crate) fn build_embedder(store_root: &std::path::Path) -> Result<Box<dyn lex_search::Embedder>> {
    if let Some(http) = lex_search::HttpEmbedder::from_env()
        .map_err(|e| anyhow!("LEX_EMBED_URL configuration: {e}"))?
    {
        let fingerprint = format!("{:?}:{}", http.provider(), http.model());
        let cache_root = lex_search::default_cache_root(store_root);
        let cached = lex_search::CachingEmbedder::new(http, cache_root, fingerprint);
        Ok(Box::new(cached))
    } else {
        Ok(Box::new(lex_search::MockEmbedder::new()))
    }
}

/// `lex plan --goal <fn> [--max-cost N] [--intent <id>] [--branch B] [--store DIR]`
/// (#307). Cost-aware path planner over the call graph. Advisory:
/// returns paths cheapest-first with a `fits` flag against the
/// effective cap; the agent (or downstream policy) decides which
/// path to apply.
fn cmd_plan(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut goal: Option<String> = None;
    let mut max_cost: Option<u64> = None;
    let mut intent: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut root: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--goal" => goal = it.next().cloned(),
            "--max-cost" => {
                max_cost = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--max-cost needs N"))?
                        .parse()
                        .map_err(|e| anyhow!("--max-cost: {e}"))?,
                );
            }
            "--intent" => intent = it.next().cloned(),
            "--branch" => branch = it.next().cloned(),
            "--store" => root = Some(std::path::PathBuf::from(
                it.next().ok_or_else(|| anyhow!("--store needs a path"))?,
            )),
            other => bail!("unexpected arg `{other}` for `lex plan`"),
        }
    }
    let goal = goal.ok_or_else(|| anyhow!(
        "usage: lex plan --goal <fn> [--max-cost N] [--intent <id>] [--branch B] [--store DIR]"))?;
    let root = root.unwrap_or_else(default_store_root);
    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    // If `--intent` is supplied, resolve its session id via the
    // IntentLog so the planner can consult `Store::session_budget`.
    let session_id = if let Some(intent_id) = &intent {
        let intent_log = lex_vcs::IntentLog::open(store.root())?;
        intent_log.get(intent_id)?.map(|i| i.session_id)
    } else {
        None
    };

    let plan = store
        .plan(&branch, &goal, max_cost, session_id.as_deref())
        .with_context(|| format!("planning paths from `{goal}` on branch `{branch}`"))?;
    let data = serde_json::to_value(&plan)?;
    acli::emit_or_text("plan", data, fmt, || {
        let cap = plan
            .effective_cap
            .map(|c| c.to_string())
            .unwrap_or_else(|| "(uncapped)".into());
        println!("plan from `{}` (effective cap: {}):", plan.goal, cap);
        if let (Some(sid), Some(r)) = (&plan.session_id, plan.remaining_budget) {
            println!("  session `{sid}`: remaining budget {r}");
        }
        if plan.paths.is_empty() {
            println!("  (no paths — goal not in the branch head's active set)");
        }
        for p in &plan.paths {
            let mark = if p.fits { "ok " } else { "no " };
            let effs = if p.effects.is_empty() {
                String::new()
            } else {
                format!(" [{}]", p.effects.iter().cloned().collect::<Vec<_>>().join(", "))
            };
            println!(
                "  {mark} cost={} {}{}",
                p.total_cost,
                p.chain.join(" -> "),
                effs,
            );
        }
    });
    Ok(())
}

/// `lex repair <op_id> [--apply --transform '<json>'] [--branch B] [--store DIR]`
/// (#281). Reads the latest `RepairHint` for the failed op_id and
/// — in `--apply` mode — executes a typed transform supplied as
/// JSON. Emits a `RepairAttempt` attestation with the outcome.
///
/// Slice 2a ships the explicit-transform path; the LLM-driven
/// path (`--apply` without `--transform`) follows in slice 2b.
fn cmd_repair(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut op_id: Option<String> = None;
    let mut root: Option<PathBuf> = None;
    let mut apply = false;
    let mut transform_json: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--store" => {
                root = Some(PathBuf::from(it.next()
                    .ok_or_else(|| anyhow!("--store needs a path"))?));
            }
            "--apply" => apply = true,
            "--transform" => {
                transform_json = Some(it.next()
                    .ok_or_else(|| anyhow!("--transform needs a JSON payload"))?
                    .clone());
            }
            "--branch" => {
                branch = Some(it.next()
                    .ok_or_else(|| anyhow!("--branch needs a name"))?.clone());
            }
            other if !other.starts_with("--") => {
                if op_id.is_some() {
                    bail!("usage: lex repair <op_id> [--apply --transform '<json>'] [--branch B] [--store DIR]");
                }
                op_id = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let op_id = op_id.ok_or_else(|| anyhow!(
        "usage: lex repair <op_id> [--apply --transform '<json>'] [--branch B] [--store DIR]"))?;
    let root = root.unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".lex/store")
    });
    let store = Store::open(&root)?;

    if apply {
        let branch = branch.unwrap_or_else(|| store.current_branch());
        // With `--transform`: slice-2a behavior — execute exactly
        // what the agent provided. Without it: slice-2b — call
        // the LLM (or fixture) to generate the transform.
        return match transform_json {
            Some(t) => cmd_repair_apply(fmt, &store, &op_id, &branch, &t),
            None => cmd_repair_apply_llm(fmt, &store, &op_id, &branch),
        };
    }
    if transform_json.is_some() {
        bail!("`--transform` requires `--apply`");
    }
    cmd_repair_read(fmt, &store, &op_id)
}

fn cmd_repair_read(
    fmt: &OutputFormat,
    store: &Store,
    op_id: &str,
) -> Result<()> {
    let attlog = store.attestation_log()
        .map_err(|e| anyhow!("opening attestation log: {e}"))?;
    let mut hits: Vec<lex_vcs::Attestation> = attlog.list_all()
        .map_err(|e| anyhow!("listing attestations: {e}"))?
        .into_iter()
        .filter(|a| matches!(&a.kind,
            lex_vcs::AttestationKind::RepairHint { failed_op_id, .. }
                if failed_op_id == op_id))
        .collect();
    hits.sort_by_key(|a| a.timestamp);
    let latest = hits.last().cloned();
    let envelope = match latest {
        Some(a) => {
            let lex_vcs::AttestationKind::RepairHint {
                failed_op_id,
                errors,
                suggested_transform,
            } = &a.kind else { unreachable!() };
            serde_json::json!({
                "found": true,
                "failed_op_id": failed_op_id,
                "stage_id": a.stage_id,
                "attestation_id": a.attestation_id,
                "timestamp": a.timestamp,
                "errors": errors,
                "suggested_transform": suggested_transform,
            })
        }
        None => serde_json::json!({
            "found": false,
            "failed_op_id": op_id,
        }),
    };
    let op_id_owned = op_id.to_string();
    acli::emit_or_text("repair", envelope.clone(), fmt, || {
        if envelope["found"] == false {
            println!("no RepairHint found for op_id `{op_id_owned}`");
        } else {
            let n = envelope["errors"].as_array()
                .map(|a| a.len()).unwrap_or(0);
            let stage = envelope["stage_id"].as_str().unwrap_or("?");
            println!("RepairHint for op_id `{op_id_owned}`:");
            println!("  stage:  {stage}");
            println!("  errors: {n}");
            println!("  suggested_transform: {}",
                if envelope["suggested_transform"].is_null() {
                    "(none — supply one via `lex repair --apply --transform ...`)".to_string()
                } else {
                    envelope["suggested_transform"].to_string()
                });
        }
    });
    Ok(())
}

/// `lex repair <op_id> --apply --transform '<json>'` — slice 2a.
///
/// Parses the transform payload (one of #280's four typed transforms)
/// and dispatches to the matching `Store::apply_*` method. The
/// outcome is recorded as a `RepairAttempt` attestation tied to the
/// original RepairHint's attestation_id so blame walks the repair
/// chain. Returns the new op_id (or pair, for ExtractFunction) on
/// success.
fn cmd_repair_apply(
    fmt: &OutputFormat,
    store: &Store,
    failed_op_id: &str,
    branch: &str,
    transform_json: &str,
) -> Result<()> {
    // Find the hint we're attesting against. Required so the
    // RepairAttempt's `hint_id` field is meaningful; without one,
    // a repair has no target to record progress against.
    let attlog = store.attestation_log()
        .map_err(|e| anyhow!("opening attestation log: {e}"))?;
    let hint = attlog.list_all()
        .map_err(|e| anyhow!("listing attestations: {e}"))?
        .into_iter()
        .filter(|a| matches!(&a.kind,
            lex_vcs::AttestationKind::RepairHint { failed_op_id: f, .. }
                if f == failed_op_id))
        .max_by_key(|a| a.timestamp)
        .ok_or_else(|| anyhow!(
            "no RepairHint exists for op_id `{failed_op_id}` — \
             a hint is required to apply a repair"
        ))?;
    let hint_attestation_id = hint.attestation_id.clone();
    let hint_stage_id = hint.stage_id.clone();

    let parsed: serde_json::Value = serde_json::from_str(transform_json)
        .with_context(|| format!("parsing --transform JSON: {transform_json}"))?;
    let kind = parsed.get("kind").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("--transform JSON missing `kind` field"))?;

    let result = dispatch_repair_transform(store, branch, &parsed, kind);
    let (outcome, applied_op_id, error_detail) = match &result {
        Ok(op_ids) => ("passed".to_string(), op_ids.first().cloned(), None),
        Err(e) => ("failed".to_string(), None, Some(format!("{e}"))),
    };

    // Emit the RepairAttempt regardless of outcome — the audit
    // trail is load-bearing whether the attempt succeeded or not.
    let attempt = lex_vcs::Attestation::new(
        hint_stage_id.clone(),
        applied_op_id.clone(),
        None,
        lex_vcs::AttestationKind::RepairAttempt {
            hint_id: hint_attestation_id.clone(),
            outcome: outcome.clone(),
            applied_op_id: applied_op_id.clone(),
        },
        if outcome == "passed" {
            lex_vcs::AttestationResult::Passed
        } else {
            lex_vcs::AttestationResult::Failed {
                detail: error_detail.clone().unwrap_or_default(),
            }
        },
        repair_attempt_producer(),
        None,
    );
    attlog.put(&attempt)
        .map_err(|e| anyhow!("recording RepairAttempt: {e}"))?;

    let env = serde_json::json!({
        "outcome": outcome,
        "hint_id": hint_attestation_id,
        "applied_op_id": applied_op_id,
        "error": error_detail,
    });
    acli::emit_or_text("repair-apply", env.clone(), fmt, || {
        match env["outcome"].as_str() {
            Some("passed") => println!(
                "repair applied: new op_id = {}",
                env["applied_op_id"].as_str().unwrap_or("?")
            ),
            _ => println!(
                "repair failed: {}",
                env["error"].as_str().unwrap_or("?")
            ),
        }
    });

    // The command itself succeeded — it ran the transform and
    // recorded a RepairAttempt. The `outcome` field in the
    // envelope and the attestation's result carry the inner
    // success/failure. Exiting non-zero would have stdout emit
    // a second wrapper envelope, which we don't want.
    Ok(())
}

/// `lex producer-trust recompute --tool <id> [--window N] [--granted-by ACTOR] [--store DIR]`
/// (#293). Walks the attestation log filtered by `produced_by.tool
/// == <id>`, computes `passed/total` over the last `window`
/// records, and emits a fresh `ProducerTrust` attestation. The
/// `required_attestations` gate consults the latest score per
/// tool to apply trust-based waivers.
fn cmd_producer_trust(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex producer-trust recompute --tool <id> [--window N] \
         [--granted-by ACTOR] [--store DIR]"))?;
    if sub != "recompute" {
        bail!("unknown `lex producer-trust` subcommand: {sub}");
    }
    let (root, rest, _, _) = parse_store_flag(&args[1..]);
    let mut tool: Option<String> = None;
    let mut window: usize = 1000;
    let mut granted_by: String = whoami_id();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--tool" => {
                tool = Some(it.next()
                    .ok_or_else(|| anyhow!("--tool needs an id"))?.clone());
            }
            "--window" => {
                window = it.next()
                    .ok_or_else(|| anyhow!("--window needs N"))?
                    .parse().map_err(|e| anyhow!("--window: {e}"))?;
            }
            "--granted-by" => {
                granted_by = it.next()
                    .ok_or_else(|| anyhow!("--granted-by needs an actor"))?.clone();
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let tool = tool.ok_or_else(|| anyhow!(
        "usage: lex producer-trust recompute --tool <id> ..."))?;

    let store = Store::open(&root)?;
    let result = store.recompute_producer_trust(&tool, window, &granted_by)?;
    let env = match &result {
        Some(att_id) => serde_json::json!({
            "tool": &tool,
            "window": window,
            "granted_by": &granted_by,
            "attestation_id": att_id,
            "ok": true,
        }),
        None => serde_json::json!({
            "tool": &tool,
            "window": window,
            "granted_by": &granted_by,
            "ok": false,
            "reason": "no attestations from this tool to score",
        }),
    };
    let env_for_text = env.clone();
    let tool_for_text = tool.clone();
    acli::emit_or_text("producer-trust", env, fmt, move || {
        if env_for_text["ok"] == true {
            println!("recomputed trust for `{tool_for_text}` → attestation_id={}",
                env_for_text["attestation_id"].as_str().unwrap_or("?"));
        } else {
            println!("no trust recompute: {}",
                env_for_text["reason"].as_str().unwrap_or("?"));
        }
    });
    Ok(())
}

/// Best-effort identity for `--granted-by`. Reads `$USER`
/// (set on Unix login shells) or falls back to `"unknown"`.
fn whoami_id() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LEX_TEA_USER"))
        .unwrap_or_else(|_| "unknown".into())
}

fn repair_attempt_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex repair".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// `lex repair <op_id> --apply` (no `--transform`) — slice 2b.
///
/// Reads the latest `RepairHint` for the failed op_id, builds a
/// structured prompt describing the four typed transforms +
/// the failure context, asks the configured LLM for a single
/// transform JSON, then hands the response off to
/// [`cmd_repair_apply`]'s machinery.
///
/// # Test infrastructure
///
/// Tests can short-circuit the LLM call by setting the
/// `LEX_REPAIR_LLM_FIXTURE` env var to a path. The contents of
/// that file replace the live LLM response. This lets the
/// subprocess-based CLI tests assert end-to-end behavior without
/// any network dependency.
fn cmd_repair_apply_llm(
    fmt: &OutputFormat,
    store: &Store,
    failed_op_id: &str,
    branch: &str,
) -> Result<()> {
    let attlog = store.attestation_log()
        .map_err(|e| anyhow!("opening attestation log: {e}"))?;
    let hint = attlog.list_all()
        .map_err(|e| anyhow!("listing attestations: {e}"))?
        .into_iter()
        .filter(|a| matches!(&a.kind,
            lex_vcs::AttestationKind::RepairHint { failed_op_id: f, .. }
                if f == failed_op_id))
        .max_by_key(|a| a.timestamp)
        .ok_or_else(|| anyhow!(
            "no RepairHint exists for op_id `{failed_op_id}` — \
             a hint is required to apply a repair"
        ))?;
    let lex_vcs::AttestationKind::RepairHint { errors, .. } = &hint.kind
        else { unreachable!() };

    let candidate_stage_id = &hint.stage_id;
    let candidate_stage = store.get_ast(candidate_stage_id)
        .map_err(|e| anyhow!("loading candidate stage `{candidate_stage_id}`: {e}"))?;
    let sig = lex_ast::sig_id(&candidate_stage)
        .ok_or_else(|| anyhow!("candidate stage has no sig_id"))?;
    let head = store.branch_head(branch)
        .map_err(|e| anyhow!("reading branch head: {e}"))?;
    let from_stage_id = head.get(&sig).cloned();
    let from_stage = match &from_stage_id {
        Some(id) => Some(store.get_ast(id)
            .map_err(|e| anyhow!("loading branch-head stage `{id}`: {e}"))?),
        None => None,
    };

    let prompt = build_repair_prompt(
        candidate_stage_id,
        &candidate_stage,
        from_stage_id.as_deref(),
        from_stage.as_ref(),
        errors,
    );
    let response = call_repair_llm(&prompt)?;
    let transform_json = response.trim().to_string();

    // Pre-validate that the response is at least parseable JSON
    // and has a `kind` field. A malformed response is recorded
    // as a `RepairAttempt` failure rather than propagated as
    // exit-non-zero — the LLM gave a bad answer; the command
    // itself processed correctly.
    let parse_err: Option<String> = match serde_json::from_str::<serde_json::Value>(&transform_json) {
        Ok(v) => {
            if v.get("kind").and_then(|x| x.as_str()).is_none() {
                Some("LLM response missing `kind` field".into())
            } else {
                None
            }
        }
        Err(e) => Some(format!("LLM response is not valid JSON: {e}")),
    };
    if let Some(reason) = parse_err {
        let attlog = store.attestation_log()
            .map_err(|e| anyhow!("opening attestation log: {e}"))?;
        let attempt = lex_vcs::Attestation::new(
            hint.stage_id.clone(),
            None,
            None,
            lex_vcs::AttestationKind::RepairAttempt {
                hint_id: hint.attestation_id.clone(),
                outcome: "failed".into(),
                applied_op_id: None,
            },
            lex_vcs::AttestationResult::Failed { detail: reason.clone() },
            repair_attempt_producer(),
            None,
        );
        attlog.put(&attempt)
            .map_err(|e| anyhow!("recording RepairAttempt: {e}"))?;
        let env = serde_json::json!({
            "outcome": "failed",
            "hint_id": hint.attestation_id,
            "applied_op_id": serde_json::Value::Null,
            "error": reason,
        });
        let env_for_text = env.clone();
        acli::emit_or_text("repair-apply", env, fmt, move || {
            println!("repair failed: {}", env_for_text["error"].as_str().unwrap_or("?"));
        });
        return Ok(());
    }

    cmd_repair_apply(fmt, store, failed_op_id, branch, &transform_json)
}

/// Build the prompt for the LLM repair call. Inlines the JSON
/// schemas for the four typed transforms so the model can choose
/// one without a separate spec fetch. Includes the candidate
/// stage (the one that didn't typecheck), the branch-head stage
/// (the one transforms should operate against), and the type
/// errors.
fn build_repair_prompt(
    candidate_stage_id: &str,
    candidate_stage: &lex_ast::Stage,
    from_stage_id: Option<&str>,
    from_stage: Option<&lex_ast::Stage>,
    errors: &serde_json::Value,
) -> String {
    let candidate_json = serde_json::to_string_pretty(candidate_stage)
        .unwrap_or_default();
    let from_json = from_stage
        .map(|s| serde_json::to_string_pretty(s).unwrap_or_default())
        .unwrap_or_else(|| "(no current branch-head stage for this sig)".into());
    let from_id_render = from_stage_id.unwrap_or("(none)");
    let errors_json = serde_json::to_string_pretty(errors)
        .unwrap_or_default();

    format!(r#"You are a Lex type-error repair assistant. The user attempted a
typed transform; the resulting stage didn't typecheck. Suggest
exactly one typed AST transform that would fix the type errors.

# Available transforms (return JSON for ONE of these)

1) replace_match_arm — replace the body of one Match arm.
{{
  "kind": "replace_match_arm",
  "from_stage_id": "<branch-head stage_id>",
  "match_node": "<NodeId of the Match>",
  "arm_index": <0-based>,
  "new_body": <CExpr JSON>
}}

2) rename_local — rename a let-bound local (scope-aware).
{{
  "kind": "rename_local",
  "from_stage_id": "<branch-head stage_id>",
  "let_node": "<NodeId of the Let>",
  "new_name": "<identifier>"
}}

3) inline_let — eliminate `let x := v; body` by substituting v.
   v must be a literal/var/field-access/binop tree (no calls,
   no side effects).
{{
  "kind": "inline_let",
  "from_stage_id": "<branch-head stage_id>",
  "let_node": "<NodeId of the Let>"
}}

4) extract_function — extract a sub-expression into a new fn.
{{
  "kind": "extract_function",
  "from_stage_id": "<branch-head stage_id>",
  "expr_node": "<NodeId of the expr>",
  "spec": {{
    "name": "<new fn name>",
    "type_params": [],
    "params": [{{"name": "n", "type": {{"node": "Named", "name": "Int", "args": []}}}}],
    "return_type": {{"node": "Named", "name": "Int", "args": []}},
    "effects": []
  }}
}}

# Failure context

Branch-head stage_id (use this as `from_stage_id`):
{from_id_render}

Branch-head stage AST (the one the transform should operate on):
{from_json}

Candidate stage_id (the one that didn't typecheck): {candidate_stage_id}

Candidate stage AST (what the agent tried; informative only):
{candidate_json}

Type errors:
{errors_json}

# Response format

Output ONLY the JSON object for your chosen transform. No prose,
no markdown fences, no surrounding commentary.
"#)
}

/// Call the configured LLM. Test escape hatch: when
/// `LEX_REPAIR_LLM_FIXTURE` is set, read the response from that
/// file instead of calling the live model. Lets the subprocess-
/// based CLI tests assert end-to-end behavior without network.
fn call_repair_llm(prompt: &str) -> Result<String> {
    if let Ok(path) = std::env::var("LEX_REPAIR_LLM_FIXTURE") {
        return std::fs::read_to_string(&path)
            .with_context(|| format!("reading LEX_REPAIR_LLM_FIXTURE at `{path}`"));
    }
    lex_runtime::llm::cloud_complete(prompt)
        .map_err(|e| anyhow!("LLM cloud_complete: {e}"))
}

/// Dispatch a `--transform` payload to the matching
/// `Store::apply_*` method. Returns the resulting op_ids
/// (singleton for the body transforms; pair for ExtractFunction).
fn dispatch_repair_transform(
    store: &Store,
    branch: &str,
    payload: &serde_json::Value,
    kind: &str,
) -> Result<Vec<lex_vcs::OpId>> {
    match kind {
        "replace_match_arm" => {
            let from = require_str(payload, "from_stage_id")?;
            let match_node = require_str(payload, "match_node")?;
            let arm_index = payload.get("arm_index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("replace_match_arm: missing arm_index"))?
                as usize;
            let new_body: lex_ast::CExpr = serde_json::from_value(
                payload.get("new_body").cloned()
                    .ok_or_else(|| anyhow!("replace_match_arm: missing new_body"))?
            ).context("parsing new_body CExpr")?;
            let op = store.apply_replace_match_arm(
                branch, from, &lex_ast::NodeId(match_node.into()),
                arm_index, new_body,
            )?;
            Ok(vec![op])
        }
        "rename_local" => {
            let from = require_str(payload, "from_stage_id")?;
            let let_node = require_str(payload, "let_node")?;
            let new_name = require_str(payload, "new_name")?;
            let op = store.apply_rename_local(
                branch, from, &lex_ast::NodeId(let_node.into()), new_name,
            )?;
            Ok(vec![op])
        }
        "inline_let" => {
            let from = require_str(payload, "from_stage_id")?;
            let let_node = require_str(payload, "let_node")?;
            let op = store.apply_inline_let(
                branch, from, &lex_ast::NodeId(let_node.into()),
            )?;
            Ok(vec![op])
        }
        "extract_function" => {
            let from = require_str(payload, "from_stage_id")?;
            let expr_node = require_str(payload, "expr_node")?;
            let spec: lex_ast::ExtractFnSpec = parse_extract_spec(
                payload.get("spec")
                    .ok_or_else(|| anyhow!("extract_function: missing spec"))?
            )?;
            let (add, modify) = store.apply_extract_function(
                branch, from, &lex_ast::NodeId(expr_node.into()), spec,
            )?;
            Ok(vec![add, modify])
        }
        other => bail!(
            "unknown transform kind `{other}` — valid kinds are \
             replace_match_arm | rename_local | inline_let | extract_function"
        ),
    }
}

fn require_str<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("--transform JSON missing string field `{key}`"))
}

/// Parse an `ExtractFnSpec` from JSON. The schema mirrors the
/// `lex_ast::ExtractFnSpec` struct field-for-field; we hand-parse
/// rather than `serde_json::from_value` because `Param`/`TypeExpr`/
/// `Effect` go through `lex-ast`'s canonical-JSON form (which is
/// the same shape, but worth being explicit).
fn parse_extract_spec(v: &serde_json::Value) -> Result<lex_ast::ExtractFnSpec> {
    let name = v.get("name").and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("extract_function spec: missing name"))?
        .to_string();
    let type_params: Vec<String> = v.get("type_params")
        .map(|x| serde_json::from_value(x.clone()))
        .transpose().context("extract_function spec.type_params")?
        .unwrap_or_default();
    let params: Vec<lex_ast::Param> = serde_json::from_value(
        v.get("params").cloned()
            .ok_or_else(|| anyhow!("extract_function spec: missing params"))?
    ).context("extract_function spec.params")?;
    let return_type: lex_ast::TypeExpr = serde_json::from_value(
        v.get("return_type").cloned()
            .ok_or_else(|| anyhow!("extract_function spec: missing return_type"))?
    ).context("extract_function spec.return_type")?;
    let effects: Vec<lex_ast::Effect> = v.get("effects")
        .map(|x| serde_json::from_value(x.clone()))
        .transpose().context("extract_function spec.effects")?
        .unwrap_or_default();
    Ok(lex_ast::ExtractFnSpec {
        name, type_params, params, return_type, effects,
    })
}

fn cmd_keygen(fmt: &OutputFormat, _args: &[String]) -> Result<()> {
    let kp = lex_vcs::Keypair::generate()
        .map_err(|e| anyhow!("keygen: {e}"))?;
    let data = serde_json::json!({
        "public_key": kp.public_hex(),
        "secret_key": kp.secret_hex(),
    });
    let pk = kp.public_hex();
    let sk = kp.secret_hex();
    acli::emit_or_text("keygen", data, fmt, move || {
        println!("public_key  {pk}");
        println!("secret_key  {sk}");
    });
    Ok(())
}

/// Resolve a signing key from the CLI flag, then env var, then None.
/// Returns `Ok(None)` if neither is set so the caller can decide
/// whether unsigned publish is allowed.
fn resolve_signing_key(flag_value: Option<&str>) -> Result<Option<lex_vcs::Keypair>> {
    let hex_str = match flag_value {
        Some(v) => Some(v.to_string()),
        None => std::env::var("LEX_SIGNING_KEY").ok(),
    };
    match hex_str {
        Some(s) if !s.is_empty() => {
            let kp = lex_vcs::Keypair::from_secret_hex(s.trim())
                .map_err(|e| anyhow!(
                    "invalid signing key (hex): {e}. \
                     Expected 64 hex chars from `lex keygen`."))?;
            Ok(Some(kp))
        }
        _ => Ok(None),
    }
}

/// Apply `--require-signed` / `--trusted-key` policy to a stage's
/// metadata. Returns `Ok(())` if the policy permits the stage:
///
/// * If `require_signed` is true and `metadata.signature` is `None`,
///   error.
/// * If `trusted_key` is set, the signature must be present, must
///   verify, and the public key must match the trusted key.
/// * If `require_signed` is true and a signature is present, the
///   signature must verify.
/// * Otherwise (no flags set, present-but-not-required signature),
///   we still verify a present signature so that a corrupted record
///   surfaces clearly rather than silently passing.
fn verify_metadata_signature(
    meta: &lex_store::Metadata,
    require_signed: bool,
    trusted_key: Option<&str>,
) -> Result<()> {
    match &meta.signature {
        None => {
            if require_signed {
                bail!("stage `{}` is not signed (--require-signed/--trusted-key was set)",
                    meta.stage_id);
            }
            Ok(())
        }
        Some(sig) => {
            lex_vcs::verify_stage_id(&meta.stage_id, sig)
                .map_err(|e| anyhow!(
                    "signature on stage `{}` failed verification: {e}",
                    meta.stage_id))?;
            if let Some(trusted) = trusted_key {
                if !sig.public_key.eq_ignore_ascii_case(trusted) {
                    bail!("stage `{}` is signed by `{}`, not by trusted key `{}`",
                        meta.stage_id, sig.public_key, trusted);
                }
            }
            Ok(())
        }
    }
}

fn cmd_canonical_decode(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!(
        "usage: lex canonical decode <bytes-file>"))?;
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow!("read {path}: {e}"))?;
    let stages = lex_ast::canonical_format::decode_program(&bytes)
        .map_err(|e| anyhow!("decode {path}: {e}"))?;
    let text = lex_ast::print_stages(&stages);
    let data = serde_json::json!({
        "ok": true,
        "stages": stages.len(),
        "text": &text,
    });
    acli::emit_or_text("canonical-decode", data, fmt, || {
        print!("{text}");
    });
    Ok(())
}

/// Tiny base64 encoder for the JSON envelope output. Avoids adding
/// a `base64` crate dep just for this CLI surface — standard
/// alphabet (RFC 4648 §4), padded.
fn encode_b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i] as usize;
        let b1 = bytes[i + 1] as usize;
        let b2 = bytes[i + 2] as usize;
        out.push(A[b0 >> 2] as char);
        out.push(A[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
        out.push(A[((b1 & 0b1111) << 2) | (b2 >> 6)] as char);
        out.push(A[b2 & 0b111111] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let b0 = bytes[i] as usize;
        out.push(A[b0 >> 2] as char);
        out.push(A[(b0 & 0b11) << 4] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let b0 = bytes[i] as usize;
        let b1 = bytes[i + 1] as usize;
        out.push(A[b0 >> 2] as char);
        out.push(A[((b0 & 0b11) << 4) | (b1 >> 4)] as char);
        out.push(A[(b1 & 0b1111) << 2] as char);
        out.push('=');
    }
    out
}

fn cmd_hash(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!("usage: lex hash <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let entries: Vec<serde_json::Value> = stages.iter().map(|s| {
        let name = stage_name(s);
        let h = stage_canonical_hash_hex(s);
        let sid = stage_id(s).unwrap_or_else(|| "-".into());
        serde_json::json!({
            "name": name,
            "canonical_ast": h,
            "stage_id": sid,
        })
    }).collect();
    let data = serde_json::json!({ "stages": entries });
    acli::emit_or_text("hash", data, fmt, || {
        for s in &stages {
            let name = stage_name(s);
            let h = stage_canonical_hash_hex(s);
            let sid = stage_id(s).unwrap_or_else(|| "-".into());
            println!("{name}\tcanonical_ast={h}\tstage_id={sid}");
        }
    });
    Ok(())
}

fn cmd_blame(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // usage: lex blame [--store DIR] [--with-evidence] <file>
    let (root, mut rest, _, _) = parse_store_flag(args);
    let with_evidence = rest.iter().any(|a| a == "--with-evidence");
    rest.retain(|a| a != "--with-evidence");
    let path = rest.first().ok_or_else(|| anyhow!("usage: lex blame [--store DIR] [--with-evidence] <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    // Attestation log is opened once per blame run (not per stage)
    // so a 1000-entry blame doesn't pay 1000 fs::create_dir_all
    // calls. The log itself is just a path holder; reads are
    // per-stage directory listings.
    let att_log = if with_evidence { Some(store.attestation_log()?) } else { None };

    let mut entries = Vec::new();
    for s in &stages {
        // Imports don't have stage identities.
        if matches!(s, Stage::Import(_)) { continue; }
        let name = stage_name(s).to_string();
        let sig = match lex_ast::sig_id(s) { Some(id) => id, None => continue };
        let here_stage = stage_id(s).unwrap_or_default();
        let history = store.sig_history(&sig)?;
        let active_stage = store.resolve_sig(&sig).ok().flatten();

        // Where does this source's stage stand?
        let here_status = history.iter().find(|h| h.stage_id == here_stage)
            .map(|h| format!("{:?}", h.status).to_lowercase());

        let history_json: Vec<serde_json::Value> = history.iter().map(|h| {
            let mut entry = serde_json::json!({
                "stage_id": h.stage_id,
                "status": format!("{:?}", h.status).to_lowercase(),
                "last_at": h.last_at,
                "published_at": h.published_at,
            });
            if let Some(log) = &att_log {
                let mut atts = log.list_for_stage(&h.stage_id).unwrap_or_default();
                atts.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert(
                        "attestations".into(),
                        serde_json::to_value(&atts).unwrap_or(serde_json::Value::Null),
                    );
                }
            }
            entry
        }).collect();
        entries.push(serde_json::json!({
            "name": name,
            "sig_id": sig,
            "here_stage_id": here_stage,
            "here_status": here_status,    // None => unpublished
            "active_stage_id": active_stage,
            "history": history_json,
        }));

        // New: causal history from the op log.
        let log = lex_vcs::OpLog::open(store.root()).ok();
        let head_op = store.get_branch(&store.current_branch()).ok()
            .and_then(|opt| opt.and_then(|b| b.head_op));
        let causal: Vec<serde_json::Value> = match (log, head_op) {
            (Some(log), Some(head)) => {
                log.walk_back(&head, None).unwrap_or_default()
                    .into_iter()
                    .filter(|r| {
                        // Touch this sig (or, for renames, produce it as the new sig).
                        match &r.op.kind {
                            lex_vcs::OperationKind::AddFunction { sig_id, .. }
                            | lex_vcs::OperationKind::ModifyBody { sig_id, .. }
                            | lex_vcs::OperationKind::ChangeEffectSig { sig_id, .. }
                            | lex_vcs::OperationKind::AddType { sig_id, .. }
                            | lex_vcs::OperationKind::ModifyType { sig_id, .. }
                            | lex_vcs::OperationKind::RemoveFunction { sig_id, .. }
                            | lex_vcs::OperationKind::RemoveType { sig_id, .. } => sig_id == &sig,
                            lex_vcs::OperationKind::RenameSymbol { from, to, .. } =>
                                from == &sig || to == &sig,
                            _ => false,
                        }
                    })
                    .map(|r| {
                        let kind_tag = serde_json::to_value(&r.op.kind).ok()
                            .and_then(|v| v.get("op").cloned())
                            .unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "op_id": r.op_id,
                            "kind": kind_tag,
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        // Mutate the most-recent entries.push value to attach causal_history.
        if let Some(last) = entries.last_mut() {
            last.as_object_mut().unwrap()
                .insert("causal_history".into(), serde_json::Value::Array(causal));
        }
    }
    let data = serde_json::json!({ "blame": entries });
    let entries_for_text = entries.clone();
    acli::emit_or_text("blame", data, fmt, move || {
        for e in &entries_for_text {
            print_blame_entry(e);
        }
    });
    Ok(())
}

fn print_blame_entry(e: &serde_json::Value) {
    let name = e["name"].as_str().unwrap_or("?");
    let sig = e["sig_id"].as_str().unwrap_or("");
    let here = e["here_stage_id"].as_str().unwrap_or("");
    let status = e["here_status"].as_str().unwrap_or("unpublished");
    let active = e["active_stage_id"].as_str();
    let history = e["history"].as_array().map(|a| a.as_slice()).unwrap_or(&[]);

    println!("fn {name}");
    println!("  sig:     {sig:.16}…");
    if active.map(|a| a == here).unwrap_or(false) {
        println!("  current: {here:.16}…  ({status})");
    } else {
        println!("  current: {here:.16}…  ({status} in store)");
        if let Some(a) = active {
            println!("  active in store: {a:.16}…");
        }
    }
    if history.is_empty() {
        println!("  history: (not in store)");
    } else {
        println!("  history: {} stage(s)", history.len());
        for h in history {
            let sid = h["stage_id"].as_str().unwrap_or("");
            let st  = h["status"].as_str().unwrap_or("?");
            let at  = h["last_at"].as_u64().unwrap_or(0);
            let marker = if sid == here { " ←" } else { "" };
            println!("    {sid:.16}…  {st:<10} {}{marker}", format_blame_ts(at));
            // `--with-evidence` attaches attestations to each history
            // entry. Render compactly: one line per attestation,
            // showing kind, result, and producer.
            if let Some(atts) = h["attestations"].as_array() {
                if atts.is_empty() {
                    println!("      evidence: (none)");
                } else {
                    for a in atts {
                        let kind = a["kind"]["kind"].as_str().unwrap_or("?");
                        let result = a["result"]["result"].as_str().unwrap_or("?");
                        let tool = a["produced_by"]["tool"].as_str().unwrap_or("?");
                        let ver = a["produced_by"]["version"].as_str().unwrap_or("?");
                        println!("      {kind:<14} {result:<8} by {tool}@{ver}");
                    }
                }
            }
        }
    }
    println!();
}

fn format_blame_ts(secs: u64) -> String {
    let mut s = secs as i64;
    let mut days = s.div_euclid(86_400);
    s = s.rem_euclid(86_400);
    let h = s / 3600; s %= 3600;
    let m = s / 60;
    let mut y: i64 = 1970;
    loop {
        let yd = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
        if days < yd { break; }
        days -= yd; y += 1;
    }
    let mdays = [31,
        if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0usize;
    while mo < 12 && days >= mdays[mo] { days -= mdays[mo]; mo += 1; }
    format!("{y:04}-{:02}-{:02}T{:02}:{:02}Z", mo + 1, days + 1, h, m)
}

fn stage_name(s: &Stage) -> &str {
    match s {
        Stage::FnDecl(fd) => &fd.name,
        Stage::TypeDecl(td) => &td.name,
        Stage::Import(i) => &i.alias,
    }
}

/// Decode a CLI argument's JSON into a `Value`. Delegates to
/// `Value::from_json` so the CLI, the `lex serve` HTTP API, and
/// in-program `json.parse` all share the same convention — including
/// `{"$variant": "Name", "args": [...]}` for variants. (Closes #93.)
fn json_to_value(v: &serde_json::Value) -> Value {
    Value::from_json(v)
}

/// Find the StageId of a function declared in `lex_src` whose name
/// matches `fn_name`. Returns `None` if the source doesn't parse,
/// the fn isn't there, or it's a non-FnDecl stage. Used by `lex
/// spec check` to tie its Spec attestation to the exact stage the
/// spec was verified against.
fn find_stage_id_for_fn(lex_src: &str, fn_name: &str) -> Option<String> {
    let prog = load_program_from_str(lex_src).ok()?;
    let stages = canonicalize_program(&prog);
    let stage = stages.iter().find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == fn_name))?;
    stage_id(stage)
}

/// Persist a `Spec` attestation against `stage_id` capturing the
/// outcome of a `lex spec check` run. Emits passed / failed (with
/// counterexample summary) / inconclusive (with note) so the
/// evidence trail covers all three verdicts — failures are
/// evidence too (#132 trust model).
fn record_spec_attestation(
    store_root: &std::path::Path,
    stage_id: &str,
    spec_name: &str,
    r: &spec_checker::CheckResult,
    trials: u32,
) -> Result<()> {
    use lex_vcs::{
        Attestation, AttestationKind, AttestationResult, ProducerDescriptor, SpecMethod,
    };
    let store = Store::open(store_root)
        .with_context(|| format!("opening store at {}", store_root.display()))?;
    let log = store.attestation_log()?;

    let result = match r.status {
        spec_checker::ProofStatus::Proved => AttestationResult::Passed,
        spec_checker::ProofStatus::Counterexample => {
            let detail = r.evidence.counterexample.as_ref()
                .and_then(|c| serde_json::to_string(c).ok())
                .map(|s| format!("counterexample: {s}"))
                .unwrap_or_else(|| "counterexample".into());
            AttestationResult::Failed { detail }
        }
        spec_checker::ProofStatus::Inconclusive => AttestationResult::Inconclusive {
            detail: r.evidence.note.clone().unwrap_or_else(|| "inconclusive".into()),
        },
    };
    let kind = AttestationKind::Spec {
        spec_id: r.spec_id.clone(),
        method: SpecMethod::Random,
        trials: Some(trials as usize),
    };
    let producer = ProducerDescriptor {
        tool: "lex spec check".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    };
    let _ = spec_name; // Reserved for future provenance fields.
    let attestation = Attestation::new(
        stage_id.to_string(),
        None,
        None,
        kind,
        result,
        producer,
        None,
    );
    log.put(&attestation)?;
    Ok(())
}

/// Emit an attestation produced by `lex agent-tool` against the
/// StageId of the agent-emitted `tool` fn. Centralizes the
/// producer descriptor so every emission site (`--spec`,
/// `--diff-body`, `--examples`, sandboxed run) tags itself
/// consistently. The `model` field carries the Claude model name
/// when the body came from `--request`; `None` for `--body`/
/// `--body-file` since the model wasn't the proximate producer.
fn emit_agent_tool_attestation(
    log: &lex_vcs::AttestationLog,
    stage_id: &str,
    kind: lex_vcs::AttestationKind,
    result: lex_vcs::AttestationResult,
    model: Option<String>,
) -> Result<()> {
    let producer = lex_vcs::ProducerDescriptor {
        tool: "lex agent-tool".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model,
    };
    let attestation = lex_vcs::Attestation::new(
        stage_id.to_string(),
        None,
        None,
        kind,
        result,
        producer,
        None,
    );
    log.put(&attestation)?;
    Ok(())
}

/// Lowercase-hex SHA-256 of `bytes`. Used by `lex agent-tool` to
/// content-hash example files and diff-body sources for the
/// `Examples`/`DiffBody` attestation kinds.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn value_to_json_string(v: &Value) -> String {
    serde_json::to_string(&v.to_json()).unwrap()
}

fn value_to_json(v: &Value) -> serde_json::Value { v.to_json() }

// ---- M6: store subcommands ----

fn default_store_root() -> PathBuf {
    if let Ok(s) = std::env::var("LEX_STORE") { return PathBuf::from(s); }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".lex").join("store");
    }
    PathBuf::from(".lex-store")
}

/// Public re-export for sibling CLI modules. `default_store_root`
/// itself stays private to keep the binary's surface tight; modules
/// that need it call this trampoline.
pub(crate) fn default_store_root_pub() -> PathBuf {
    default_store_root()
}

fn parse_store_flag(args: &[String]) -> (PathBuf, Vec<String>, bool, bool) {
    // Returns (store_root, remaining_args, activate, dry_run).
    let mut root = default_store_root();
    let mut activate = false;
    let mut dry_run = false;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--store" => {
                if let Some(v) = args.get(i + 1) { root = PathBuf::from(v); i += 2; } else { i += 1; }
            }
            "--activate" => { activate = true; i += 1; }
            "--dry-run" => { dry_run = true; i += 1; }
            _ => { rest.push(args[i].clone()); i += 1; }
        }
    }
    (root, rest, activate, dry_run)
}

fn cmd_publish(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    use lex_vcs::ImportMap;

    let (root, rest, activate, dry_run) = parse_store_flag(args);
    // Pull --branch and --signing-key off as well.
    let mut branch: Option<String> = None;
    let mut signing_key_flag: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else if a == "--signing-key" {
            signing_key_flag = Some(it.next()
                .ok_or_else(|| anyhow!("--signing-key needs a hex value"))?.clone());
        } else {
            positional.push(a.clone());
        }
    }
    let path = positional.first().ok_or_else(|| anyhow!(
        "usage: lex publish [--store DIR] [--branch NAME] [--activate] [--signing-key HEX] <file>"))?;
    let signer = resolve_signing_key(signing_key_flag.as_deref())?;

    let prog = read_program(path)?;
    // #168: type-check *and* rewrite stdlib parse calls so a
    // typed `toml.parse[T]` validates required fields before
    // returning Ok. The mutation lands in the canonical AST so
    // every downstream consumer (bytecode compile, store
    // publish) sees the strict shape.
    let mut stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
        let arr: Vec<serde_json::Value> = errs.iter()
            .map(|e| serde_json::to_value(e).unwrap()).collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("publish", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) { eprintln!("{j}"); }
            }
        });
        std::process::exit(2);
    }

    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    // Compute the diff. We need the old fns and new fns.
    let old_head = store.branch_head(&branch)?;
    let old_fns: BTreeMap<String, lex_ast::FnDecl> = old_head.values()
        .filter_map(|stg| store.get_ast(stg).ok())
        .filter_map(|s| match s { Stage::FnDecl(fd) => Some((fd.name.clone(), fd)), _ => None })
        .collect();
    let new_fns: BTreeMap<String, lex_ast::FnDecl> = stages.iter()
        .filter_map(|s| match s { Stage::FnDecl(fd) => Some((fd.name.clone(), fd.clone())), _ => None })
        .collect();
    let report = diff::compute_diff(&old_fns, &new_fns, /* body_patches: */ true);

    // Build new imports map (one entry per source file we just read).
    let mut new_imports: ImportMap = ImportMap::new();
    // Stable, transport-independent key. Per-file imports are not
    // currently tracked separately — all imports of one publish are
    // grouped under "<source>" so that publishing the same source
    // via CLI vs HTTP produces identical op_ids.
    let file_key = "<source>".to_string();
    let entry = new_imports.entry(file_key).or_default();
    for s in &stages {
        if let Stage::Import(im) = s {
            entry.insert(im.reference.clone());
        }
    }

    if dry_run {
        // Compute the op kinds for the dry-run preview using diff_to_ops
        // directly, without persisting anything.
        let old_name_to_sig: BTreeMap<String, String> = old_head.iter()
            .filter_map(|(sig, stg)| {
                store.get_metadata(stg).ok().map(|m| (m.name, sig.clone()))
            })
            .collect();
        let old_effects: BTreeMap<String, BTreeSet<String>> = old_head.iter()
            .filter_map(|(sig, stg)| {
                let ast = store.get_ast(stg).ok()?;
                match ast {
                    Stage::FnDecl(fd) => {
                        let s: BTreeSet<String> = fd.effects.iter()
                            .map(|e| e.name.clone()).collect();
                        Some((sig.clone(), s))
                    }
                    _ => None,
                }
            })
            .collect();
        let old_imports = store.derive_imports_from_oplog(&branch)?;
        let op_kinds = lex_vcs::diff_to_ops(lex_vcs::DiffInputs {
            old_head: &old_head,
            old_name_to_sig: &old_name_to_sig,
            old_effects: &old_effects,
            old_imports: &old_imports,
            new_stages: &stages,
            new_imports: &new_imports,
            diff: &report,
        }).map_err(|e| anyhow!("diff_to_ops: {e}"))?;
        let actions: Vec<serde_json::Value> = op_kinds.iter()
            .map(|k| serde_json::to_value(k).unwrap())
            .collect();
        acli::emit_dry_run("publish", fmt,
            &format!("would apply {} op(s) to branch {}", op_kinds.len(), branch),
            actions);
        return Ok(());
    }

    let outcome = store.publish_program_signed(
        &branch, &stages, &report, &new_imports, activate, signer.as_ref())?;
    let signed = signer.as_ref().map(|kp| kp.public_hex());
    let data = serde_json::json!({
        "ops": outcome.ops,
        "head_op": outcome.head_op,
        "signed_by": signed,
    });
    acli::emit_or_text("publish", data, fmt, || {});
    Ok(())
}

fn cmd_store(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!("usage: lex store {{list|get}} ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "list" => {
            let (root, _rest, _, _) = parse_store_flag(rest);
            let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
            let sigs = store.list_sigs()?;
            let entries: Vec<serde_json::Value> = sigs.iter().map(|s| {
                let active = store.resolve_sig(s).ok().flatten().unwrap_or_default();
                serde_json::json!({ "sig_id": s, "active_stage_id": active })
            }).collect();
            let data = serde_json::json!({ "sigs": entries });
            acli::emit_or_text("store", data, fmt, || {
                for s in &sigs {
                    let active = store.resolve_sig(s).ok().flatten().unwrap_or_default();
                    println!("{s}\tactive={active}");
                }
            });
            Ok(())
        }
        "get" => {
            let (root, rest, _, _) = parse_store_flag(rest);
            let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
            // #227 verification flags. `--require-signed` rejects an
            // unsigned stage; `--trusted-key HEX` rejects any stage
            // whose signature was made by a different key. Both are
            // independent: `--trusted-key` implies signed.
            let mut require_signed = false;
            let mut trusted_key: Option<String> = None;
            let mut positional: Vec<&String> = Vec::new();
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                if a == "--require-signed" {
                    require_signed = true;
                } else if a == "--trusted-key" {
                    trusted_key = Some(it.next()
                        .ok_or_else(|| anyhow!("--trusted-key needs a hex value"))?
                        .clone());
                    require_signed = true;
                } else {
                    positional.push(a);
                }
            }
            let id = positional.first()
                .ok_or_else(|| anyhow!(
                    "usage: lex store get [--require-signed] [--trusted-key HEX] <stage_id>"))?;
            let meta = store.get_metadata(id)?;
            verify_metadata_signature(&meta, require_signed, trusted_key.as_deref())?;
            let ast = store.get_ast(id)?;
            let v = serde_json::json!({
                "metadata": serde_json::to_value(&meta)?,
                "status": format!("{:?}", store.get_status(id)?).to_lowercase(),
                "ast": serde_json::to_value(&ast)?,
                "signature_verified": meta.signature.is_some(),
            });
            acli::emit_or_text("store", v.clone(), fmt, || {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            });
            Ok(())
        }
        "search" => cmd_store_search(fmt, rest),
        "migrate-ops" => cmd_store_migrate_ops(fmt, rest),
        other => bail!("unknown `lex store` subcommand: {other}"),
    }
}

/// `lex store migrate-ops` (#244). Re-canonicalize every op in the
/// store under a target [`OperationFormat`]. Today only V1 exists,
/// so the production migration is always a no-op; the command
/// surfaces the plan/apply mechanism that future format bumps will
/// rely on.
///
/// Flags:
/// * `--to v1` (required) — the target format. Future variants will
///   accept their own tags.
/// * `--dry-run` — print the old→new mapping without rewriting any
///   files. Mutually exclusive with `--confirm`.
/// * `--confirm` — apply the migration. **Destructive**: deletes
///   the old `<root>/ops/<old_op_id>.json` files and rewrites
///   `<root>/branches/*.json` so `head_op` references the new ids.
///   Attestations are *not* rewritten in this slice — see #244 and
///   the attestation cascade follow-up.
fn cmd_store_migrate_ops(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // `parse_store_flag` already consumes `--dry-run` and returns it
    // as the 4th tuple element; we honor that, not a re-parse from
    // the remainder.
    let (root, rest, _activate, dry_run) = parse_store_flag(args);
    let mut target_str: Option<String> = None;
    let mut confirm = false;
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--to" => {
                target_str = Some(iter.next()
                    .ok_or_else(|| anyhow!("--to needs a format tag (today: v1)"))?
                    .clone());
            }
            "--confirm" => confirm = true,
            other => bail!("unknown flag `{other}` for `lex store migrate-ops`"),
        }
    }
    if dry_run && confirm {
        bail!("--dry-run and --confirm are mutually exclusive");
    }
    if !dry_run && !confirm {
        bail!(
            "lex store migrate-ops is destructive — pass --dry-run to preview, \
             --confirm to apply"
        );
    }
    let target_str = target_str
        .ok_or_else(|| anyhow!("--to <format> is required (today: v1)"))?;
    let target: lex_vcs::OperationFormat = match target_str.as_str() {
        "v1" | "V1" => lex_vcs::OperationFormat::V1,
        other => bail!("unknown operation format `{other}` — supported: v1"),
    };

    let log = lex_vcs::OpLog::open(&root)
        .with_context(|| format!("opening op log at {}", root.display()))?;
    let plan = lex_vcs::migrate::plan_migration(&log, target)
        .with_context(|| "planning migration")?;

    let mapping = plan.mapping();
    let changed: Vec<&lex_vcs::migrate::MigrationStep> = plan
        .steps
        .iter()
        .filter(|s| s.old_op_id != s.new_op_id)
        .collect();

    let mappings_json: Vec<serde_json::Value> = plan
        .steps
        .iter()
        .map(|s| {
            serde_json::json!({
                "old": s.old_op_id,
                "new": s.new_op_id,
                "changed": s.old_op_id != s.new_op_id,
            })
        })
        .collect();

    let summary = serde_json::json!({
        "target_format": format!("{:?}", target).to_lowercase(),
        "total_ops": plan.steps.len(),
        "rotated_op_ids": changed.len(),
        "is_no_op": plan.is_no_op(),
        "applied": false,
        "mappings": mappings_json,
    });

    if dry_run {
        acli::emit_or_text("store-migrate-ops", summary.clone(), fmt, || {
            println!(
                "would migrate {} ops to {:?}; {} op_ids would rotate (dry-run, no files written)",
                plan.steps.len(),
                target,
                changed.len(),
            );
            for s in &plan.steps {
                if s.old_op_id != s.new_op_id {
                    println!("  {} → {}", s.old_op_id, s.new_op_id);
                }
            }
            if !changed.is_empty() {
                println!(
                    "\nNote: applying with --confirm will also rewrite branch heads \
                     and cascade-migrate attestations whose `op_id` rotated (#258)."
                );
            }
        });
        return Ok(());
    }

    // --confirm path: apply.
    lex_vcs::migrate::apply_migration(&log, &plan)
        .with_context(|| "applying op-log migration")?;

    let branch_updates = rewrite_branch_heads(&root, &mapping)
        .with_context(|| "rewriting branch heads")?;

    // #258: cascade migrate attestations whose `op_id` references
    // a rotated op. Their `attestation_id` is computed including
    // op_id, so they all rotate too.
    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let attest_log = store.attestation_log()
        .with_context(|| "opening attestation log")?;
    let att_steps = lex_vcs::migrate::plan_attestation_migration(&attest_log, &mapping)
        .with_context(|| "planning attestation cascade")?;
    lex_vcs::migrate::apply_attestation_migration(&attest_log, &att_steps)
        .with_context(|| "applying attestation cascade")?;
    let attestations_rotated = att_steps.iter().filter(|s| !s.is_no_op()).count();

    // Invalidate the gate-checkpoint pointers on every branch
    // (#256). They reference op_ids by content, which the
    // migration just rotated; without invalidation the next
    // advance would compare against a stale id and re-walk
    // unnecessarily (or, worse, treat the new head as "already
    // verified" because its old name happened to match).
    let _ = store.invalidate_gate_checkpoints();

    let summary = serde_json::json!({
        "target_format": format!("{:?}", target).to_lowercase(),
        "total_ops": plan.steps.len(),
        "rotated_op_ids": changed.len(),
        "is_no_op": plan.is_no_op(),
        "applied": true,
        "branches_updated": branch_updates,
        "attestations_rotated": attestations_rotated,
        "mappings": summary["mappings"].clone(),
    });
    acli::emit_or_text("store-migrate-ops", summary, fmt, || {
        println!(
            "migrated {} ops to {:?}; {} op_ids rotated; \
             {} branch heads rewritten; {} attestations cascade-migrated",
            plan.steps.len(),
            target,
            changed.len(),
            branch_updates,
            attestations_rotated,
        );
    });
    Ok(())
}

/// Walk `<root>/branches/*.json`, parse each, and rewrite `head_op`
/// in place if the current value appears in `mapping`. Returns the
/// number of branch files that changed.
///
/// Bypasses `lex-store`'s `set_branch_head_op` (which is `pub(crate)`)
/// because this is a one-shot supervised rewrite invoked by the
/// `migrate-ops` command — not a normal write path.
fn rewrite_branch_heads(
    root: &std::path::Path,
    mapping: &std::collections::BTreeMap<String, String>,
) -> Result<usize> {
    let dir = root.join("branches");
    if !dir.exists() {
        return Ok(0);
    }
    let mut updated = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let mut value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        let mut changed = false;
        if let Some(head) = value.get("head_op").and_then(|v| v.as_str()) {
            if let Some(new) = mapping.get(head) {
                value["head_op"] = serde_json::Value::String(new.clone());
                changed = true;
            }
        }
        if changed {
            let new_bytes = serde_json::to_vec_pretty(&value)
                .with_context(|| format!("serializing {}", path.display()))?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &new_bytes)?;
            std::fs::rename(&tmp, &path)?;
            updated += 1;
        }
    }
    Ok(updated)
}

/// `lex store search "<query>"` (#224). Embeds the query and ranks
/// every active stage in the store by fused cosine similarity over
/// description + signature + examples. Slice 1 ships only the
/// MockEmbedder for offline / deterministic ranking; the network-
/// backed providers gate on `LEX_EMBED_URL` (slice 2).
fn cmd_store_search(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // `lex store search reindex` warms the embedding cache by
    // walking every active stage through `SearchIndex::build`
    // (#283). Falls through to query mode for any non-reindex
    // positional.
    if matches!(args.first().map(String::as_str), Some("reindex")) {
        return cmd_store_search_reindex(fmt, &args[1..]);
    }
    let (root, rest, _, _) = parse_store_flag(args);
    let mut limit: usize = 10;
    let mut query: Option<String> = None;
    let mut iter = rest.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--limit" => {
                let v = iter.next().ok_or_else(|| anyhow!("--limit needs a value"))?;
                limit = v.parse().context("--limit must be a positive integer")?;
            }
            other if !other.starts_with("--") => {
                if query.is_some() {
                    bail!("usage: lex store search [--limit N] \"<query>\"");
                }
                query = Some(other.to_string());
            }
            other => bail!("unknown flag `{other}` for `lex store search`"),
        }
    }
    let query = query.ok_or_else(|| anyhow!(
        "usage: lex store search [--limit N] \"<query>\""))?;

    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let embedder = build_embedder(&root)?;
    let idx = lex_search::SearchIndex::build(&store, &*embedder)
        .map_err(|e| anyhow!("building search index: {e}"))?;
    let hits = idx.query(&*embedder, &query, limit)
        .map_err(|e| anyhow!("query embedding: {e}"))?;
    let v = serde_json::json!({
        "query": &query,
        "limit": limit,
        "indexed": idx.stages.len(),
        "hits": serde_json::to_value(&hits)?,
    });
    acli::emit_or_text("store-search", v.clone(), fmt, || {
        println!("{} hit(s) for `{}`", hits.len(), query);
        for h in &hits {
            println!(
                "  {:>6.3}  {}::{}  {}",
                h.score.fused, h.stage_id, h.name, h.signature,
            );
            if let Some(d) = &h.description { println!("          note: {d}"); }
        }
    });
    Ok(())
}

/// `lex store search reindex [--store DIR]` (#283). Walks every
/// active stage through the configured embedder, populating the
/// on-disk cache so subsequent `lex store search <query>` calls
/// don't pay the embedding cost on the cold path.
///
/// With `LEX_EMBED_URL` set, this calls the HTTP backend (Ollama or
/// OpenAI-compat per `LEX_EMBED_PROVIDER`); without it, falls back
/// to [`lex_search::MockEmbedder`] (fast but semantically random —
/// useful for warming a deterministic test fixture).
///
/// Emits `{ indexed, dim, embedder, store }` as the JSON envelope.
fn cmd_store_search_reindex(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, _rest, _, _) = parse_store_flag(args);
    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let embedder = build_embedder(&root)?;
    let started = std::time::Instant::now();
    let idx = lex_search::SearchIndex::build(&store, &*embedder)
        .map_err(|e| anyhow!("building search index: {e}"))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let v = serde_json::json!({
        "indexed": idx.stages.len(),
        "dim": embedder.dim(),
        "elapsed_ms": elapsed_ms,
        "store": root.display().to_string(),
    });
    acli::emit_or_text("store-search-reindex", v.clone(), fmt, || {
        println!("indexed {} stage(s) ({}-dim embeddings, {} ms)",
            idx.stages.len(), embedder.dim(), elapsed_ms);
    });
    Ok(())
}

/// `lex stage <stage_id>` — print metadata + canonical AST + status.
/// `lex stage <stage_id> --attestations` — list every attestation
/// for the stage, newest-first by timestamp. CLI mirror of
/// `GET /v1/stage/<id>` and `GET /v1/stage/<id>/attestations`.
fn cmd_stage(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    // `lex stage pin|defer|block|unblock <id> ...` — human triage
    // actions (#172). Detect them as a leading positional so the
    // existing `lex stage <id>` and `lex stage <id> --attestations`
    // shapes keep working unchanged.
    if let Some(action) = rest.first().map(String::as_str) {
        match action {
            "pin"     => return cmd_stage_pin(fmt, &root, &rest[1..]),
            "defer"   => return cmd_stage_decision(
                fmt, &root, &rest[1..], StageDecision::Defer),
            "block"   => return cmd_stage_decision(
                fmt, &root, &rest[1..], StageDecision::Block),
            "unblock" => return cmd_stage_decision(
                fmt, &root, &rest[1..], StageDecision::Unblock),
            // #294: multi-agent coordination.
            "candidates" => return cmd_stage_candidates(fmt, &root, &rest[1..]),
            "promote-candidate" => return cmd_stage_promote_candidate(fmt, &root, &rest[1..]),
            _ => {}
        }
    }
    let attestations_mode = rest.iter().any(|a| a == "--attestations");
    let id = rest
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow!("usage: lex stage <stage_id> [--attestations]"))?;
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;

    if attestations_mode {
        // 404-equivalent: refuse to list against an unknown stage so
        // callers can't silently get an empty list for a typo.
        store
            .get_metadata(id)
            .with_context(|| format!("unknown stage `{id}`"))?;
        let log = store.attestation_log()?;
        let mut listing = log.list_for_stage(id)?;
        listing.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
        let data = serde_json::json!({
            "stage_id": id,
            "attestations": serde_json::to_value(&listing)?,
        });
        acli::emit_or_text("stage", data, fmt, move || {
            if listing.is_empty() {
                println!("(no attestations)");
                return;
            }
            for a in &listing {
                let kind = match &a.kind {
                    lex_vcs::AttestationKind::TypeCheck => "TypeCheck".to_string(),
                    lex_vcs::AttestationKind::EffectAudit => "EffectAudit".to_string(),
                    lex_vcs::AttestationKind::Examples { count, .. } => {
                        format!("Examples({count})")
                    }
                    lex_vcs::AttestationKind::Spec { spec_id, .. } => {
                        format!("Spec({spec_id})")
                    }
                    lex_vcs::AttestationKind::DiffBody { input_count, .. } => {
                        format!("DiffBody({input_count})")
                    }
                    lex_vcs::AttestationKind::SandboxRun { effects } => {
                        let joined: Vec<&str> = effects.iter().map(String::as_str).collect();
                        format!("SandboxRun([{}])", joined.join(","))
                    }
                    lex_vcs::AttestationKind::Override { actor, .. } => {
                        format!("Override({actor})")
                    }
                    lex_vcs::AttestationKind::Defer { actor, .. } => {
                        format!("Defer({actor})")
                    }
                    lex_vcs::AttestationKind::Block { actor, .. } => {
                        format!("Block({actor})")
                    }
                    lex_vcs::AttestationKind::Unblock { actor, .. } => {
                        format!("Unblock({actor})")
                    }
                    lex_vcs::AttestationKind::Trace { run_id, root_target } => {
                        format!("Trace({root_target}@{run_id:.12}…)")
                    }
                    lex_vcs::AttestationKind::ProducerBlock { tool_id, .. } => {
                        format!("ProducerBlock({tool_id})")
                    }
                    lex_vcs::AttestationKind::ProducerUnblock { tool_id, .. } => {
                        format!("ProducerUnblock({tool_id})")
                    }
                    lex_vcs::AttestationKind::RepairHint { failed_op_id, .. } => {
                        format!("RepairHint({failed_op_id:.12}…)")
                    }
                    lex_vcs::AttestationKind::RepairAttempt { hint_id, outcome, .. } => {
                        format!("RepairAttempt({outcome}, {hint_id:.12}…)")
                    }
                    lex_vcs::AttestationKind::ProducerTrust { tool_id, score_thousandths, .. } => {
                        format!("ProducerTrust({tool_id}, {:.3})", *score_thousandths as f64 / 1000.0)
                    }
                    lex_vcs::AttestationKind::TrustWaived { producer, kind_tag, .. } => {
                        format!("TrustWaived({producer}/{kind_tag})")
                    }
                };
                let result = match &a.result {
                    lex_vcs::AttestationResult::Passed => "passed".to_string(),
                    lex_vcs::AttestationResult::Failed { detail } => format!("failed: {detail}"),
                    lex_vcs::AttestationResult::Inconclusive { detail } => format!("inconclusive: {detail}"),
                };
                println!(
                    "{}\t{}\t{}\tby={}@{}",
                    a.timestamp, kind, result, a.produced_by.tool, a.produced_by.version,
                );
            }
        });
        return Ok(());
    }

    // Default: stage info, mirroring `GET /v1/stage/<id>`.
    let meta = store.get_metadata(id)?;
    let ast = store.get_ast(id)?;
    let status = format!("{:?}", store.get_status(id)?).to_lowercase();
    let v = serde_json::json!({
        "metadata": serde_json::to_value(&meta)?,
        "ast": serde_json::to_value(&ast)?,
        "status": status,
    });
    acli::emit_or_text("stage", v.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    });
    Ok(())
}

/// Resolve and validate the actor for a triage action.
/// Combines `--actor` and `LEX_TEA_USER` (in that order), and
/// when `<root>/users.json` exists requires the resulting name
/// to be in the file. Returns a printable error mentioning the
/// command verb so the user can see which surface refused them.
fn resolve_actor(
    root: &std::path::Path,
    supplied: Option<String>,
    verb: &str,
) -> Result<String> {
    let actor = supplied
        .or_else(|| std::env::var("LEX_TEA_USER").ok())
        .ok_or_else(|| anyhow!(
            "lex stage {verb}: actor unknown — pass --actor NAME or set LEX_TEA_USER"
        ))?;
    if let Some(users) = lex_store::users::load(root)
        .with_context(|| format!("reading users.json at {}", root.display()))?
    {
        if !users.knows(&actor) {
            bail!(
                "lex stage {verb}: actor `{actor}` not listed in {}/users.json",
                root.display()
            );
        }
    }
    Ok(actor)
}

/// `lex stage pin <id> --reason "..." [--actor <name>]` —
/// human override (#172, lex-tea v3a). Activates the stage and
/// records an `Override` attestation alongside whatever
/// existing attestations the stage already has. The pin
/// itself is auditable: `lex attest filter --kind override`
/// returns every override the human(s) have issued.
///
/// `actor` defaults to `$LEX_TEA_USER`; falling back errors so
/// `lex stage candidates <sig_id> [--store DIR]` (#294). Lists
/// every live `Candidate` op for the sig — those not yet
/// referenced as winner or in `supersedes` by any `Promote`.
/// Sorted by op_id for reproducibility.
fn cmd_stage_candidates(
    fmt: &OutputFormat,
    root: &std::path::Path,
    rest: &[String],
) -> Result<()> {
    let sig_id = rest.first().ok_or_else(|| anyhow!(
        "usage: lex stage candidates <sig_id> [--store DIR]"))?;
    let store = Store::open(root)?;
    let candidates = store.list_candidates(sig_id)?;
    let data = serde_json::json!({
        "sig_id": sig_id,
        "candidates": &candidates,
        "count": candidates.len(),
    });
    let sig_for_text = sig_id.clone();
    let printable = candidates.clone();
    acli::emit_or_text("stage-candidates", data, fmt, move || {
        if printable.is_empty() {
            println!("(no live candidates for `{sig_for_text}`)");
            return;
        }
        println!("{} candidate(s) for `{sig_for_text}`:", printable.len());
        for c in &printable {
            let intent = c.intent_id.as_deref().unwrap_or("(none)");
            println!("  op_id={:.16}…  stage_id={:.16}…  intent={:.16}…",
                c.op_id, c.stage_id, intent);
        }
    });
    Ok(())
}

/// `lex stage promote-candidate <candidate_op_id> [--branch B]
/// [--store DIR]` (#294). Emits a `Promote` op advancing the
/// branch head with the candidate's stage. Every other live
/// candidate for the same sig is listed in `supersedes` so the
/// op log explicitly records the bake-off.
fn cmd_stage_promote_candidate(
    fmt: &OutputFormat,
    root: &std::path::Path,
    rest: &[String],
) -> Result<()> {
    let mut op_id: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--branch" => {
                branch = Some(it.next()
                    .ok_or_else(|| anyhow!("--branch needs a name"))?.clone());
            }
            other if !other.starts_with("--") => {
                if op_id.is_some() {
                    bail!("usage: lex stage promote-candidate <op_id> [--branch B]");
                }
                op_id = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let op_id = op_id.ok_or_else(|| anyhow!(
        "usage: lex stage promote-candidate <op_id> [--branch B] [--store DIR]"))?;
    let store = Store::open(root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());
    let new_op_id = store.promote_candidate(&branch, &op_id)?;
    let data = serde_json::json!({
        "promoted_candidate": op_id,
        "new_op_id": new_op_id,
        "branch": branch,
    });
    let candidate_for_text = op_id.clone();
    let new_id_for_text = new_op_id.clone();
    let branch_for_text = branch.clone();
    acli::emit_or_text("stage-promote-candidate", data, fmt, move || {
        println!("promoted candidate `{candidate_for_text}` on `{branch_for_text}`");
        println!("  new op_id: {new_id_for_text}");
    });
    Ok(())
}

/// a pin can't land anonymously. When `<store>/users.json`
/// exists, the resolved name must be in the file (v3d, #172).
fn cmd_stage_pin(
    fmt: &OutputFormat,
    root: &std::path::Path,
    args: &[String],
) -> Result<()> {
    let mut id: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reason" => { reason = args.get(i + 1).cloned(); i += 2; }
            "--actor"  => { actor  = args.get(i + 1).cloned(); i += 2; }
            other if id.is_none() => { id = Some(other.to_string()); i += 1; }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let id = id.ok_or_else(|| anyhow!(
        "usage: lex stage pin <stage_id> --reason \"...\" [--actor NAME]"
    ))?;
    let reason = reason.ok_or_else(|| anyhow!(
        "lex stage pin: --reason required (overrides need a paper trail)"
    ))?;
    let actor = resolve_actor(root, actor, "pin")?;

    let store = Store::open(root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    // Verify the stage exists; refuse to pin something that's not
    // even there so a typo can't accidentally activate the wrong
    // sig later.
    let _ = store.get_metadata(&id)
        .with_context(|| format!("unknown stage `{id}`"))?;

    // Refuse to pin a blocked stage. The block is only meaningful
    // if it actually stops the activation it's supposed to prevent.
    let log = store.attestation_log()?;
    let existing = log.list_for_stage(&id)?;
    if lex_vcs::is_stage_blocked(&existing) {
        bail!(
            "lex stage pin: stage `{id}` is blocked — run `lex stage unblock {id} --reason \"...\"` first"
        );
    }

    // Activate first (the actual override action), then record the
    // audit. Order matters: if activate fails, no audit is written;
    // if audit fails after a successful activate, the user retries
    // and the attestation_id is content-addressed so re-puts dedup.
    store.activate(&id)
        .with_context(|| format!("activate stage `{id}`"))?;

    let producer = lex_vcs::ProducerDescriptor {
        tool: "lex stage pin".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    };
    let attestation = lex_vcs::Attestation::new(
        id.clone(), None, None,
        lex_vcs::AttestationKind::Override {
            actor: actor.clone(),
            reason: reason.clone(),
            target_attestation_id: None,
        },
        // Override is a *fact* about the human's choice, not a
        // pass/fail of code. Use Passed for "the override was
        // recorded successfully" — Failed/Inconclusive don't
        // apply to administrative actions.
        lex_vcs::AttestationResult::Passed,
        producer, None,
    );
    let log = store.attestation_log()?;
    log.put(&attestation)?;

    let data = serde_json::json!({
        "pinned": &id,
        "actor": &actor,
        "reason": &reason,
        "attestation_id": &attestation.attestation_id,
    });
    let id_for_text = id.clone();
    let actor_for_text = actor.clone();
    acli::emit_or_text("stage", data, fmt, move || {
        println!("→ pinned `{id_for_text:.16}…` (actor: {actor_for_text})");
    });
    Ok(())
}

/// Triage decisions a human can record on a stage. Mirrors the
/// `Defer`/`Block`/`Unblock` `AttestationKind` variants.
#[derive(Clone, Copy)]
enum StageDecision {
    Defer,
    Block,
    Unblock,
}

impl StageDecision {
    fn verb(self) -> &'static str {
        match self {
            Self::Defer => "defer",
            Self::Block => "block",
            Self::Unblock => "unblock",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Self::Defer => "deferred",
            Self::Block => "blocked",
            Self::Unblock => "unblocked",
        }
    }
}

/// `lex stage <defer|block|unblock> <id> --reason "..." [--actor NAME]`
/// — human triage actions (#172, lex-tea v3b).
///
/// Defer/Block/Unblock all record an attestation against the stage
/// without changing its status. Block additionally makes future
/// `lex stage pin` calls refuse until an `unblock` is recorded.
/// The append-only attestation log makes the full triage history
/// queryable via `lex attest filter --kind block` etc.
fn cmd_stage_decision(
    fmt: &OutputFormat,
    root: &std::path::Path,
    args: &[String],
    decision: StageDecision,
) -> Result<()> {
    let mut id: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reason" => { reason = args.get(i + 1).cloned(); i += 2; }
            "--actor"  => { actor  = args.get(i + 1).cloned(); i += 2; }
            other if id.is_none() => { id = Some(other.to_string()); i += 1; }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let verb = decision.verb();
    let id = id.ok_or_else(|| anyhow!(
        "usage: lex stage {verb} <stage_id> --reason \"...\" [--actor NAME]"
    ))?;
    let reason = reason.ok_or_else(|| anyhow!(
        "lex stage {verb}: --reason required (triage decisions need a paper trail)"
    ))?;
    let actor = resolve_actor(root, actor, verb)?;

    let store = Store::open(root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let _ = store.get_metadata(&id)
        .with_context(|| format!("unknown stage `{id}`"))?;

    let kind = match decision {
        StageDecision::Defer => lex_vcs::AttestationKind::Defer {
            actor: actor.clone(), reason: reason.clone(),
        },
        StageDecision::Block => lex_vcs::AttestationKind::Block {
            actor: actor.clone(), reason: reason.clone(),
        },
        StageDecision::Unblock => lex_vcs::AttestationKind::Unblock {
            actor: actor.clone(), reason: reason.clone(),
        },
    };
    let producer = lex_vcs::ProducerDescriptor {
        tool: format!("lex stage {verb}"),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    };
    let attestation = lex_vcs::Attestation::new(
        id.clone(), None, None,
        kind,
        lex_vcs::AttestationResult::Passed,
        producer, None,
    );
    let log = store.attestation_log()?;
    log.put(&attestation)?;

    let data = serde_json::json!({
        "stage_id": &id,
        "decision": verb,
        "actor": &actor,
        "reason": &reason,
        "attestation_id": &attestation.attestation_id,
    });
    let id_for_text = id.clone();
    let actor_for_text = actor.clone();
    let past = decision.past();
    acli::emit_or_text("stage", data, fmt, move || {
        println!("→ {past} `{id_for_text:.16}…` (actor: {actor_for_text})");
    });
    Ok(())
}

/// `lex attest filter --kind K --result R --since T --store DIR`
/// — cross-stage attestation query (#132). Walks every primary
/// attestation file under `<store>/attestations/` and filters by
/// the supplied criteria. Designed for CI / dashboard queries
/// that span the whole log rather than a single stage.
fn cmd_attest(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex attest {{filter|push}} ..."
    ))?;
    let rest = &args[1..];
    if sub == "push" {
        return cmd_attest_push(fmt, rest);
    }
    if sub == "pull" {
        return cmd_attest_pull(fmt, rest);
    }
    match sub.as_str() {
        "filter" => {
            let mut kind_filter: Option<String> = None;
            let mut result_filter: Option<String> = None;
            let mut since: Option<u64> = None;
            let mut store_root: Option<PathBuf> = None;
            // #246: `--run <id>` filters to Trace attestations whose
            // `kind.run_id` matches. Implies `--kind trace`.
            let mut run_filter: Option<String> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--kind" => {
                        kind_filter = rest.get(i + 1).cloned();
                        i += 2;
                    }
                    "--result" => {
                        result_filter = rest.get(i + 1).cloned();
                        i += 2;
                    }
                    "--since" => {
                        let raw = rest.get(i + 1).ok_or_else(|| anyhow!("--since needs a value"))?;
                        since = Some(parse_since(raw)
                            .ok_or_else(|| anyhow!("--since must be epoch seconds or YYYY-MM-DD, got `{raw}`"))?);
                        i += 2;
                    }
                    "--store" => {
                        store_root = rest.get(i + 1).map(PathBuf::from);
                        i += 2;
                    }
                    "--run" => {
                        run_filter = rest.get(i + 1).cloned();
                        i += 2;
                    }
                    other => bail!("unexpected arg `{other}`"),
                }
            }
            let root = store_root.unwrap_or_else(default_store_root);
            let store = Store::open(&root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            let log = store.attestation_log()?;
            // `--run` uses the by-run secondary index instead of
            // walking every attestation; this is `O(traces of that
            // run)` rather than `O(all attestations)`.
            let all = match &run_filter {
                Some(rid) => log.list_for_run(rid)?,
                None => log.list_all()?,
            };

            let mut filtered: Vec<lex_vcs::Attestation> = all.into_iter()
                .filter(|a| {
                    if let Some(k) = &kind_filter {
                        if attestation_kind_tag(&a.kind) != *k {
                            return false;
                        }
                    }
                    if let Some(r) = &result_filter {
                        if attestation_result_tag(&a.result) != *r {
                            return false;
                        }
                    }
                    if let Some(s) = since {
                        if a.timestamp < s { return false; }
                    }
                    true
                })
                .collect();
            filtered.sort_by_key(|a| std::cmp::Reverse(a.timestamp));

            let data = serde_json::json!({
                "count": filtered.len(),
                "attestations": serde_json::to_value(&filtered)?,
            });
            let printable = filtered.clone();
            acli::emit_or_text("attest", data, fmt, move || {
                if printable.is_empty() {
                    println!("(no attestations match)");
                    return;
                }
                for a in &printable {
                    let kind = attestation_kind_tag(&a.kind);
                    let result = attestation_result_tag(&a.result);
                    println!(
                        "{}\t{}\t{}\t{:.16}…\tby={}@{}",
                        a.timestamp, kind, result, a.stage_id,
                        a.produced_by.tool, a.produced_by.version,
                    );
                }
            });
            Ok(())
        }
        "retro-block" => cmd_attest_retro_block(fmt, rest),
        "retro-unblock" => cmd_attest_retro_unblock(fmt, rest),
        other => bail!("unknown `lex attest` subcommand: {other}"),
    }
}

/// `lex attest retro-block --producer <tool_id> --reason "..."` (#248).
/// Emits an `AttestationKind::ProducerBlock` attestation under
/// `stage_id == tool_id`. The branch advance gate consults the
/// resulting record on every subsequent apply and refuses to
/// advance over an op whose stage carries an attestation produced
/// by `tool_id` at or after this block's timestamp.
fn cmd_attest_retro_block(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut producer: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--producer" => {
                producer = rest.get(i + 1).cloned();
                i += 2;
            }
            "--reason" => {
                reason = rest.get(i + 1).cloned();
                i += 2;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let tool_id = producer.ok_or_else(|| anyhow!(
        "usage: lex attest retro-block --producer <tool_id> --reason \"...\""
    ))?;
    let reason = reason.ok_or_else(|| anyhow!(
        "lex attest retro-block: --reason required"
    ))?;

    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let attestation = lex_vcs::Attestation::with_timestamp(
        tool_id.clone(),
        None,
        None,
        lex_vcs::AttestationKind::ProducerBlock {
            tool_id: tool_id.clone(),
            reason: reason.clone(),
            blocked_at: now,
        },
        // The verdict on the *block itself* is always Passed —
        // it's a declaration, not the result of a verification.
        // Failure to land the attestation surfaces as an io error,
        // not as `Failed { detail }`.
        lex_vcs::AttestationResult::Passed,
        retro_block_producer(),
        None,
        now,
    );
    log.put(&attestation)?;
    // #256: a fresh ProducerBlock invalidates every branch's
    // walk-back gate checkpoint, forcing the next advance to
    // re-walk the chain and discover any contamination.
    let invalidated = store.invalidate_gate_checkpoints()
        .with_context(|| "invalidating gate checkpoints after retro-block")?;

    let data = serde_json::json!({
        "tool_id": &tool_id,
        "reason": &reason,
        "blocked_at": now,
        "attestation_id": &attestation.attestation_id,
        "branches_invalidated": invalidated,
    });
    let printable_tool = tool_id.clone();
    acli::emit_or_text("attest", data, fmt, move || {
        println!("→ retroactively blocked producer `{printable_tool}` at {now}");
    });
    Ok(())
}

/// `lex attest retro-unblock --producer <tool_id> --reason "..."` (#248).
/// Counterpart to `retro-block`. Emits an
/// `AttestationKind::ProducerUnblock` so the gate honors the most
/// recent verdict per `tool_id` by timestamp.
fn cmd_attest_retro_unblock(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut producer: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--producer" => {
                producer = rest.get(i + 1).cloned();
                i += 2;
            }
            "--reason" => {
                reason = rest.get(i + 1).cloned();
                i += 2;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let tool_id = producer.ok_or_else(|| anyhow!(
        "usage: lex attest retro-unblock --producer <tool_id> --reason \"...\""
    ))?;
    let reason = reason.ok_or_else(|| anyhow!(
        "lex attest retro-unblock: --reason required"
    ))?;

    let store = Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let attestation = lex_vcs::Attestation::with_timestamp(
        tool_id.clone(),
        None,
        None,
        lex_vcs::AttestationKind::ProducerUnblock {
            tool_id: tool_id.clone(),
            reason: reason.clone(),
            unblocked_at: now,
        },
        lex_vcs::AttestationResult::Passed,
        retro_block_producer(),
        None,
        now,
    );
    log.put(&attestation)?;
    // #256: an unblock can also unblock previously-refused branch
    // advances. Invalidate so the next advance re-walks and lets
    // through anything the unblock cleared.
    let invalidated = store.invalidate_gate_checkpoints()
        .with_context(|| "invalidating gate checkpoints after retro-unblock")?;

    let data = serde_json::json!({
        "tool_id": &tool_id,
        "reason": &reason,
        "unblocked_at": now,
        "attestation_id": &attestation.attestation_id,
        "branches_invalidated": invalidated,
    });
    let printable_tool = tool_id.clone();
    acli::emit_or_text("attest", data, fmt, move || {
        println!("→ retroactively unblocked producer `{printable_tool}` at {now}");
    });
    Ok(())
}

/// Producer descriptor for the synthetic `ProducerBlock` /
/// `ProducerUnblock` attestations written by the `lex attest
/// retro-{block,unblock}` commands. Tagged distinctly from
/// `lex run --trace`'s `trace_producer` so the activity feed can
/// tell the two apart.
fn retro_block_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex attest retro-block".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// `lex attest push <remote_url> [--since-op OP_ID] [--store DIR]
/// [--dry-run]` (#242).
///
/// Walks the local attestation log, optionally filtering to
/// attestations whose `op_id` is `>= --since-op` (in DAG order, not
/// timestamp), and posts them to `<remote_url>/v1/attestations/batch`.
///
/// Without `--since-op`, sends every attestation. The server-side
/// idempotency check (content-addressed `attestation_id`) makes
/// "push everything" safe; `--since-op` is purely an optimization
/// for large logs.
///
/// Idempotency: re-pushing the same attestations converges to
/// `added: 0`. Network failure mid-push leaves the remote with the
/// prefix that landed; re-running picks up where the failure
/// occurred.
fn cmd_attest_push(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut remote: Option<String> = None;
    let mut since_op: Option<String> = None;
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since-op" => {
                since_op = Some(it.next().ok_or_else(|| anyhow!("--since-op needs an op_id"))?.clone());
            }
            "--dry-run" => dry_run = true,
            other if !other.starts_with("--") && remote.is_none() => {
                remote = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let remote = remote.ok_or_else(|| anyhow!(
        "usage: lex attest push <remote_url> [--since-op OP_ID] [--dry-run] [--store DIR]"
    ))?;

    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;

    // Filter by op-id ancestry when --since-op is set: only push
    // attestations whose op_id is reachable from the local op log
    // and not in the ancestry of `since_op`. Without --since-op,
    // send every attestation.
    let all = log.list_all()?;
    let to_send: Vec<lex_vcs::Attestation> = match since_op.as_ref() {
        None => all,
        Some(cutoff) => {
            let op_log = lex_vcs::OpLog::open(&root)?;
            // Set of op_ids we should NOT re-send: every ancestor of
            // `cutoff`, inclusive.
            let exclude: std::collections::BTreeSet<String> =
                op_log.walk_back(cutoff, None)?
                    .into_iter()
                    .map(|r| r.op_id)
                    .collect();
            all.into_iter()
                .filter(|a| match &a.op_id {
                    Some(op_id) => !exclude.contains(op_id),
                    None => true,
                })
                .collect()
        }
    };

    if dry_run {
        let ids: Vec<&String> = to_send.iter().map(|a| &a.attestation_id).collect();
        let data = serde_json::json!({
            "remote": remote,
            "since_op": since_op,
            "would_send": to_send.len(),
            "attestation_ids": ids,
        });
        let count = to_send.len();
        let remote_text = remote.clone();
        acli::emit_or_text("attest-push", data, fmt, move || {
            println!("would push {count} attestations to {remote_text} (dry-run)");
        });
        return Ok(());
    }

    if to_send.is_empty() {
        let data = serde_json::json!({
            "remote": remote,
            "received": 0,
            "added": 0,
            "skipped": 0,
        });
        acli::emit_or_text("attest-push", data, fmt, || {
            println!("nothing to push (no attestations match)");
        });
        return Ok(());
    }

    let url = format!("{}/v1/attestations/batch", remote.trim_end_matches('/'));
    let body = serde_json::to_string(&to_send)
        .map_err(|e| anyhow!("serializing batch: {e}"))?;
    let resp = ureq::post(&url)
        .header("Content-Type", "application/json")
        .send(body)
        .map_err(|e| anyhow!("POST {url}: {e}"))?;
    let status = resp.status().as_u16();
    let resp_body: serde_json::Value = resp.into_body().read_json()
        .map_err(|e| anyhow!("decoding response: {e}"))?;
    if status >= 400 {
        bail!("server rejected batch (HTTP {status}): {resp_body}");
    }

    let received = resp_body.get("received").and_then(|v| v.as_u64()).unwrap_or(0);
    let added = resp_body.get("added").and_then(|v| v.as_u64()).unwrap_or(0);
    let skipped = resp_body.get("skipped").and_then(|v| v.as_u64()).unwrap_or(0);
    let data = serde_json::json!({
        "remote": remote,
        "received": received,
        "added": added,
        "skipped": skipped,
    });
    let remote_text = remote.clone();
    acli::emit_or_text("attest-push", data, fmt, move || {
        println!(
            "pushed {received} attestations to {remote_text}: \
             {added} added, {skipped} skipped (already present)"
        );
    });
    Ok(())
}

/// `lex attest pull <remote_url> [--since-op OP_ID] [--limit N]
/// [--dry-run] [--store DIR]` (#260).
///
/// Append-only fetch of attestations — the inverse of `lex attest
/// push`. Asks the remote for attestations whose `op_id` is not in
/// the ancestry of `--since-op` (or, without the flag, whose
/// `op_id` we don't already know about), validates each, and
/// persists.
///
/// Idempotency: re-running converges to `added: 0`. Network failure
/// mid-pull leaves the local with the prefix that landed; the next
/// run picks up where the failure occurred.
fn cmd_attest_pull(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut remote: Option<String> = None;
    let mut since_op: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut dry_run = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--since-op" => {
                since_op = Some(it.next().ok_or_else(|| anyhow!("--since-op needs an op_id"))?.clone());
            }
            "--limit" => {
                limit = Some(it.next().ok_or_else(|| anyhow!("--limit needs N"))?
                    .parse().map_err(|e| anyhow!("--limit: {e}"))?);
            }
            "--dry-run" => dry_run = true,
            other if !other.starts_with("--") && remote.is_none() => {
                remote = Some(other.to_string());
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let remote = remote.ok_or_else(|| anyhow!(
        "usage: lex attest pull <remote_url> [--since-op OP_ID] [--limit N] [--dry-run] [--store DIR]"
    ))?;

    let mut url = format!(
        "{}/v1/attestations/since",
        remote.trim_end_matches('/'),
    );
    let mut sep = '?';
    if let Some(op) = &since_op {
        url.push_str(&format!("{sep}after-op={op}"));
        sep = '&';
    }
    if let Some(n) = limit {
        url.push_str(&format!("{sep}limit={n}"));
    }
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp.into_body().read_to_string()
            .unwrap_or_else(|_| "(unreadable body)".into());
        bail!("server returned HTTP {status}: {body}");
    }
    let received: Vec<lex_vcs::Attestation> = resp.into_body().read_json()
        .map_err(|e| anyhow!("decoding response from {url}: {e}"))?;

    if dry_run {
        let ids: Vec<&String> = received.iter().map(|a| &a.attestation_id).collect();
        let data = serde_json::json!({
            "remote": remote,
            "since_op": since_op,
            "would_receive": received.len(),
            "attestation_ids": ids,
        });
        let count = received.len();
        let remote_text = remote.clone();
        acli::emit_or_text("attest-pull", data, fmt, move || {
            println!("would pull {count} attestations from {remote_text} (dry-run)");
        });
        return Ok(());
    }

    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let log = store.attestation_log()?;
    let op_log = lex_vcs::OpLog::open(&root)?;

    let mut added = 0usize;
    let mut rejected_unknown_op = 0usize;
    for att in &received {
        // Validate content-addressing.
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
            bail!(
                "remote returned attestation with mismatched id: supplied={}, expected={}",
                att.attestation_id, expected,
            );
        }
        // If the attestation references an op_id, that op must
        // already exist locally — otherwise the attestation is
        // dangling. Skip rather than fail the whole pull; the
        // caller can re-issue after pulling the missing ops.
        if let Some(op_id) = &att.op_id {
            if op_log.get(op_id)?.is_none() {
                rejected_unknown_op += 1;
                continue;
            }
        }
        let was_present = log.get(&att.attestation_id)?.is_some();
        log.put(att)?;
        if !was_present {
            added += 1;
        }
    }

    let data = serde_json::json!({
        "remote": remote,
        "received": received.len(),
        "added": added,
        "skipped": received.len() - added - rejected_unknown_op,
        "rejected_unknown_op": rejected_unknown_op,
    });
    let total = received.len();
    let skipped = received.len() - added - rejected_unknown_op;
    let remote_text = remote.clone();
    acli::emit_or_text("attest-pull", data, fmt, move || {
        println!(
            "pulled {total} attestations from {remote_text}: \
             {added} new, {skipped} already present, {rejected_unknown_op} skipped (unknown op_id)"
        );
    });
    Ok(())
}

fn attestation_kind_tag(k: &lex_vcs::AttestationKind) -> &'static str {
    use lex_vcs::AttestationKind::*;
    match k {
        Examples { .. }         => "examples",
        Spec { .. }             => "spec",
        DiffBody { .. }         => "diff_body",
        TypeCheck               => "type_check",
        EffectAudit             => "effect_audit",
        SandboxRun { .. }       => "sandbox_run",
        Override { .. }         => "override",
        Defer { .. }            => "defer",
        Block { .. }            => "block",
        Unblock { .. }          => "unblock",
        Trace { .. }            => "trace",
        ProducerBlock { .. }    => "producer_block",
        ProducerUnblock { .. }  => "producer_unblock",
        RepairHint { .. }       => "repair_hint",
        RepairAttempt { .. }    => "repair_attempt",
        ProducerTrust { .. }    => "producer_trust",
        TrustWaived { .. }      => "trust_waived",
    }
}

/// `lex policy {block-producer|unblock-producer|list}` — manage
/// the local trust policy at `<store>/policy.json` (#181). The
/// list is consulted at attestation-read time: producers on it
/// keep their attestations in the log (audit trail intact) but
/// the activity feed and other consumers tag those rows
/// `blocked`. Enforcement is local; nothing is mutated in the
/// attestation log itself.
fn cmd_policy(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex policy {{block-producer <name> --reason \"...\" | unblock-producer <name> | \
         require-attestation <kind> [--when-effects e1,e2,...] | unrequire-attestation <kind> | \
         session-budget {{set-default <N> | set <id> <N> | unbounded <id> | clear <id> | clear-default}} | \
         list | show}} [--store DIR]"
    ))?;
    let rest = &args[1..];
    match sub.as_str() {
        "block-producer"        => cmd_policy_block(fmt, rest),
        "unblock-producer"      => cmd_policy_unblock(fmt, rest),
        "require-attestation"   => cmd_policy_require_attestation(fmt, rest),
        "unrequire-attestation" => cmd_policy_unrequire_attestation(fmt, rest),
        "session-budget"        => cmd_policy_session_budget(fmt, rest),
        // `show` is the new name; `list` is kept as an alias for the
        // pre-#245 muscle memory.
        "list" | "show"         => cmd_policy_show(fmt, rest),
        other => bail!("unknown `lex policy` subcommand: {other}"),
    }
}

/// `lex policy session-budget <subcmd>` — manage
/// `policy.session_budgets` (#292 slices 2 + 3).
fn cmd_policy_session_budget(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex policy session-budget {{set-default <N> | set <id> <N> | \
         unbounded <id> | clear <id> | clear-default}} [--store DIR]"
    ))?;
    let (root, rest, _, _) = parse_store_flag(&args[1..]);
    let mut policy = lex_store::policy::load(&root)
        .map_err(|e| anyhow!("loading policy.json: {e}"))?
        .unwrap_or_default();
    let action;
    match sub.as_str() {
        "set-default" => {
            let n: u64 = rest.first().ok_or_else(|| anyhow!(
                "usage: lex policy session-budget set-default <N>"))?
                .parse().map_err(|e| anyhow!("invalid N: {e}"))?;
            policy.session_budgets.default_cap = Some(n);
            action = format!("set default_cap to {n}");
        }
        "set" => {
            let id = rest.first().ok_or_else(|| anyhow!(
                "usage: lex policy session-budget set <session_id> <N>"))?
                .clone();
            let n: u64 = rest.get(1).ok_or_else(|| anyhow!(
                "usage: lex policy session-budget set <session_id> <N>"))?
                .parse().map_err(|e| anyhow!("invalid N: {e}"))?;
            policy.session_budgets.overrides.insert(id.clone(), Some(n));
            action = format!("set override `{id}` to {n}");
        }
        "unbounded" => {
            let id = rest.first().ok_or_else(|| anyhow!(
                "usage: lex policy session-budget unbounded <session_id>"))?
                .clone();
            policy.session_budgets.overrides.insert(id.clone(), None);
            action = format!("set override `{id}` to unbounded");
        }
        "clear" => {
            let id = rest.first().ok_or_else(|| anyhow!(
                "usage: lex policy session-budget clear <session_id>"))?;
            policy.session_budgets.overrides.remove(id);
            action = format!("cleared override `{id}`");
        }
        "clear-default" => {
            policy.session_budgets.default_cap = None;
            action = "cleared default_cap".into();
        }
        other => bail!("unknown `session-budget` subcommand: {other}"),
    }
    lex_store::policy::save(&root, &policy)
        .map_err(|e| anyhow!("writing policy.json: {e}"))?;
    let action_for_text = action.clone();
    let data = serde_json::json!({
        "action": action,
        "session_budgets": serde_json::to_value(&policy.session_budgets)?,
    });
    acli::emit_or_text("policy", data, fmt, move || {
        println!("policy.session_budgets: {action_for_text}");
    });
    Ok(())
}

fn cmd_policy_block(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut name: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--reason" => { reason = rest.get(i + 1).cloned(); i += 2; }
            other if name.is_none() && !other.starts_with("--") => {
                name = Some(other.to_string()); i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let name = name.ok_or_else(|| anyhow!(
        "usage: lex policy block-producer <name> --reason \"...\""
    ))?;
    let reason = reason.ok_or_else(|| anyhow!(
        "lex policy block-producer: --reason required"
    ))?;
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let was_already_blocked = policy.is_blocked(&name);
    policy.block(name.clone(), reason.clone(), now);
    lex_store::policy::save(&root, &policy)
        .with_context(|| format!("writing policy.json at {}", root.display()))?;

    let data = serde_json::json!({
        "tool": &name,
        "reason": &reason,
        "blocked_at": now,
        "newly_blocked": !was_already_blocked,
    });
    let name_for_text = name.clone();
    acli::emit_or_text("policy", data, fmt, move || {
        if was_already_blocked {
            println!("(already blocked) {name_for_text}");
        } else {
            println!("→ blocked producer `{name_for_text}`");
        }
    });
    Ok(())
}

fn cmd_policy_unblock(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let name = rest.iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow!("usage: lex policy unblock-producer <name>"))?
        .clone();
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let removed = policy.unblock(&name);
    if removed {
        lex_store::policy::save(&root, &policy)
            .with_context(|| format!("writing policy.json at {}", root.display()))?;
    }
    let data = serde_json::json!({
        "tool": &name,
        "was_blocked": removed,
    });
    let name_for_text = name.clone();
    acli::emit_or_text("policy", data, fmt, move || {
        if removed {
            println!("→ unblocked producer `{name_for_text}`");
        } else {
            println!("(not blocked) {name_for_text}");
        }
    });
    Ok(())
}

/// `lex policy show` (formerly `lex policy list`) — render every
/// active rule in `policy.json`. Covers both the negative
/// `blocked_producers` gate (#181) and the positive
/// `required_attestations` gate (#245).
fn cmd_policy_show(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, _rest, _, _) = parse_store_flag(args);
    let policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let required_json: Vec<serde_json::Value> = policy
        .required_attestations
        .iter()
        .map(|r| match &r.when {
            lex_store::policy::AttestationCondition::Always => serde_json::json!({
                "kind": r.kind.tag(),
                "when": "always",
            }),
            lex_store::policy::AttestationCondition::EffectsIntersect(effects) => {
                serde_json::json!({
                    "kind": r.kind.tag(),
                    "when": "effects_intersect",
                    "effects": effects.iter().collect::<Vec<_>>(),
                })
            }
        })
        .collect();
    let data = serde_json::json!({
        "blocked_producers": &policy.blocked_producers,
        // `count` is the pre-#245 key (count of blocked producers).
        // Kept under that name so external `lex policy list --output
        // json` consumers don't break; new `blocked_count` /
        // `required_count` are the explicit, namespaced versions.
        "count": policy.blocked_producers.len(),
        "blocked_count": policy.blocked_producers.len(),
        "required_attestations": required_json,
        "required_count": policy.required_attestations.len(),
    });
    let blocked = policy.blocked_producers.clone();
    let required = policy.required_attestations.clone();
    acli::emit_or_text("policy", data, fmt, move || {
        println!("# blocked producers");
        if blocked.is_empty() {
            println!("(none)");
        } else {
            for p in &blocked {
                println!("{}\tsince={}\treason={}", p.tool, p.blocked_at, p.reason);
            }
        }
        println!("\n# required attestations");
        if required.is_empty() {
            println!("(none)");
        } else {
            for r in &required {
                match &r.when {
                    lex_store::policy::AttestationCondition::Always => {
                        println!("{}\twhen=always", r.kind.tag());
                    }
                    lex_store::policy::AttestationCondition::EffectsIntersect(effects) => {
                        let list = effects.iter().cloned().collect::<Vec<_>>().join(",");
                        println!("{}\twhen=effects_intersect({list})", r.kind.tag());
                    }
                }
            }
        }
    });
    Ok(())
}

/// `lex policy require-attestation <kind> [--when-effects e1,e2,...]`
/// (#245). Adds a positive gate rule. Without `--when-effects`, the
/// rule fires on every op (`AttestationCondition::Always`); with it,
/// the rule only fires when the op's declared effect set intersects
/// the listed effects.
fn cmd_policy_require_attestation(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let mut kind_str: Option<String> = None;
    let mut effects: Option<std::collections::BTreeSet<String>> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--when-effects" => {
                let v = rest.get(i + 1)
                    .ok_or_else(|| anyhow!("--when-effects needs a comma-separated list"))?;
                effects = Some(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
                i += 2;
            }
            other if kind_str.is_none() && !other.starts_with("--") => {
                kind_str = Some(other.to_string());
                i += 1;
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }
    let kind_str = kind_str.ok_or_else(|| anyhow!(
        "usage: lex policy require-attestation <kind> [--when-effects e1,e2,...]\n\
         supported kinds: type_check, spec, sandbox_run, examples, diff_body, effect_audit"
    ))?;
    let kind = lex_store::policy::RequiredAttestationKind::from_tag(&kind_str)
        .ok_or_else(|| anyhow!("unknown attestation kind `{kind_str}`"))?;
    let when = match effects {
        Some(set) => lex_store::policy::AttestationCondition::EffectsIntersect(set),
        None => lex_store::policy::AttestationCondition::Always,
    };
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let added = policy.require_attestation(kind, when.clone());
    lex_store::policy::save(&root, &policy)
        .with_context(|| format!("writing policy.json at {}", root.display()))?;

    let when_json = match &when {
        lex_store::policy::AttestationCondition::Always => serde_json::json!({"always": null}),
        lex_store::policy::AttestationCondition::EffectsIntersect(set) => {
            serde_json::json!({"effects_intersect": set.iter().collect::<Vec<_>>()})
        }
    };
    let data = serde_json::json!({
        "kind": kind.tag(),
        "when": when_json,
        "newly_added": added,
    });
    let kind_tag = kind.tag();
    acli::emit_or_text("policy", data, fmt, move || {
        if added {
            println!("→ require attestation `{kind_tag}`");
        } else {
            println!("(already required) {kind_tag}");
        }
    });
    Ok(())
}

/// `lex policy unrequire-attestation <kind>` (#245). Removes every
/// rule with the given kind. To narrow a rule (Always → effects),
/// unrequire then re-require.
fn cmd_policy_unrequire_attestation(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
    let kind_str = rest.iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow!("usage: lex policy unrequire-attestation <kind>"))?
        .clone();
    let kind = lex_store::policy::RequiredAttestationKind::from_tag(&kind_str)
        .ok_or_else(|| anyhow!("unknown attestation kind `{kind_str}`"))?;
    let mut policy = lex_store::policy::load(&root)
        .with_context(|| format!("reading policy.json at {}", root.display()))?
        .unwrap_or_default();
    let removed = policy.unrequire_attestation(kind);
    if removed > 0 {
        lex_store::policy::save(&root, &policy)
            .with_context(|| format!("writing policy.json at {}", root.display()))?;
    }
    let data = serde_json::json!({
        "kind": kind.tag(),
        "removed": removed,
    });
    let kind_tag = kind.tag();
    acli::emit_or_text("policy", data, fmt, move || {
        if removed > 0 {
            println!("→ removed {removed} rule(s) for `{kind_tag}`");
        } else {
            println!("(no rules) {kind_tag}");
        }
    });
    Ok(())
}

fn attestation_result_tag(r: &lex_vcs::AttestationResult) -> &'static str {
    use lex_vcs::AttestationResult::*;
    match r {
        Passed              => "passed",
        Failed { .. }       => "failed",
        Inconclusive { .. } => "inconclusive",
    }
}

/// Accept either Unix epoch seconds (a u64) or `YYYY-MM-DD`. The
/// date form resolves to start-of-day UTC. Returns `None` on a
/// shape we don't recognize so the caller can surface a friendly
/// usage error.
fn parse_since(s: &str) -> Option<u64> {
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 { return None; }
    let y: i64 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 { return None; }
    if y < 1970 { return None; }

    let mut days: i64 = 0;
    for yr in 1970..y {
        let yd = if (yr % 4 == 0 && yr % 100 != 0) || yr % 400 == 0 { 366 } else { 365 };
        days += yd;
    }
    let leap_year = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [31, if leap_year { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mi = (m - 1) as usize;
    if d > mdays[mi] as u32 { return None; }
    days += mdays.iter().take(mi).sum::<i64>();
    days += (d - 1) as i64;
    Some((days as u64) * 86_400)
}

fn cmd_replay(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    // usage: lex replay <run_id> <file> <fn> [args] [--override NODE=JSON]
    let mut overrides: indexmap::IndexMap<String, serde_json::Value> = indexmap::IndexMap::new();
    let mut policy = Policy::pure();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--override" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--override needs NODE=JSON"))?;
                let (node, json) = val.split_once('=').ok_or_else(|| anyhow!("--override expects NODE=JSON"))?;
                let v: serde_json::Value = serde_json::from_str(json)
                    .with_context(|| format!("--override value must be JSON: {json}"))?;
                overrides.insert(node.to_string(), v);
                i += 2;
            }
            "--allow-effects" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                policy.allow_effects = val.split(',').filter(|s| !s.is_empty())
                    .map(|s| s.to_string()).collect::<BTreeSet<_>>();
                i += 2;
            }
            _ => { positional.push(args[i].clone()); i += 1; }
        }
    }
    let _orig_run_id = positional.first().ok_or_else(|| anyhow!("usage: lex replay <run_id> <file> <fn> [args]"))?;
    let path = positional.get(1).ok_or_else(|| anyhow!("missing <file>"))?;
    let func = positional.get(2).ok_or_else(|| anyhow!("missing <fn>"))?;

    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let arr: Vec<serde_json::Value> = errs.iter()
            .map(|e| serde_json::to_value(e).unwrap()).collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("replay", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) { eprintln!("{j}"); }
            }
        });
        std::process::exit(2);
    }
    let bc = compile_program(&stages);
    if let Err(violations) = check_policy(&bc, &policy) {
        let arr: Vec<serde_json::Value> = violations.iter()
            .map(|v| serde_json::to_value(v).unwrap()).collect();
        let data = serde_json::json!({ "phase": "policy", "violations": arr });
        acli::emit_or_text("replay", data, fmt, || {
            for v in &violations {
                if let Ok(j) = serde_json::to_string(v) { eprintln!("{j}"); }
            }
        });
        std::process::exit(3);
    }

    let recorder = lex_trace::Recorder::new().with_overrides(overrides);
    let handle = recorder.handle();
    let bc = std::sync::Arc::new(bc);
    let handler = DefaultHandler::new(policy).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.set_tracer(Box::new(recorder));

    let vargs: Vec<Value> = positional[3..].iter().map(|a| {
        let v: serde_json::Value = serde_json::from_str(a)
            .with_context(|| format!("arg `{a}` must be JSON"))?;
        Ok(json_to_value(&v))
    }).collect::<Result<Vec<_>>>()?;

    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let result = vm.call(func, vargs);
    let ended = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    let store = lex_store::Store::open(default_store_root())?;
    let (root_out, root_err) = match &result {
        Ok(v) => (Some(value_to_json(v)), None),
        Err(e) => (None, Some(format!("{e}"))),
    };
    let tree = handle.finalize(func.clone(), serde_json::Value::Null, root_out, root_err, started, ended);
    let new_run_id = store.save_trace(&tree)?;
    if !matches!(fmt, OutputFormat::Json) { eprintln!("trace saved: {new_run_id}"); }
    let r = result.map_err(|e| anyhow!("runtime: {e}"))?;
    let data = serde_json::json!({
        "result": value_to_json(&r),
        "trace_id": new_run_id,
    });
    acli::emit_or_text("replay", data, fmt, || println!("{}", value_to_json_string(&r)));
    Ok(())
}

fn cmd_serve(args: &[String]) -> Result<()> {
    let mut port: u16 = 4040;
    let mut store_root = default_store_root();
    let mut mcp = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--port needs value"))?;
                port = v.parse().context("--port must be u16")?;
                i += 2;
            }
            "--store" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--store needs path"))?;
                store_root = std::path::PathBuf::from(v);
                i += 2;
            }
            "--mcp" => { mcp = true; i += 1; }
            _ => i += 1,
        }
    }
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

fn cmd_conformance(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let dir = args.first().ok_or_else(|| anyhow!("usage: lex conformance <dir>"))?;
    let report = conformance::run_directory(dir).context("reading conformance directory")?;
    let total = report.total();
    let passed_n = report.passed.len();
    let failed: Vec<serde_json::Value> = report.failed.iter()
        .map(|(n, w)| serde_json::json!({ "name": n, "reason": w })).collect();
    let data = serde_json::json!({
        "passed": &report.passed,
        "failed": failed,
        "total": total,
        "passed_count": passed_n,
        "ok": report.ok(),
    });
    acli::emit_or_text("conformance", data, fmt, || {
        for name in &report.passed { println!("PASS  {name}"); }
        for (name, why) in &report.failed { println!("FAIL  {name}: {why}"); }
        println!();
        println!("{}/{} passed", passed_n, total);
    });
    if report.ok() { Ok(()) } else { std::process::exit(4); }
}

fn cmd_spec(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!("usage: lex spec {{check|smt}} ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "check" => {
            let mut spec_path: Option<&String> = None;
            let mut src_path: Option<&String> = None;
            let mut trials: u32 = 1000;
            let mut store_root: Option<PathBuf> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--source" => { src_path = rest.get(i + 1); i += 2; }
                    "--trials" => {
                        trials = rest.get(i + 1).and_then(|s| s.parse().ok())
                            .ok_or_else(|| anyhow!("--trials needs a u32"))?;
                        i += 2;
                    }
                    "--store" => {
                        store_root = rest.get(i + 1).map(PathBuf::from);
                        i += 2;
                    }
                    _ if spec_path.is_none() => { spec_path = Some(&rest[i]); i += 1; }
                    other => bail!("unexpected arg `{other}`"),
                }
            }
            let spec_path = spec_path.ok_or_else(|| anyhow!("usage: lex spec check <spec> --source <file>"))?;
            let src_path = src_path.ok_or_else(|| anyhow!("--source <file> required"))?;
            let spec_src = read_source(spec_path)?;
            let lex_src = read_source(src_path)?;
            let spec = spec_checker::parse_spec(&spec_src)
                .map_err(|e| anyhow!("spec parse: {e}"))?;
            let r = spec_checker::check_spec(&spec, &lex_src, trials);

            // #132: when --store is provided, emit a Spec attestation
            // tied to the StageId of the function the spec targets.
            // The attestation captures the verification result
            // (passed / failed-with-counterexample / inconclusive)
            // so a downstream `lex blame --with-evidence` or
            // `GET /v1/stage/<id>/attestations` answers "has this
            // stage ever been spec-checked?" without re-running.
            //
            // No-ops if `--store` is absent or the source doesn't
            // contain a fn matching `spec.name` (the typical case
            // is a spec referring to a fn that *is* in the source).
            if let Some(root) = &store_root {
                if let Some(target_stage_id) = find_stage_id_for_fn(&lex_src, &spec.name) {
                    record_spec_attestation(root, &target_stage_id, &spec.name, &r, trials)?;
                }
            }

            let data = serde_json::to_value(&r)?;
            acli::emit_or_text("spec", data.clone(), fmt, || {
                println!("{}", serde_json::to_string_pretty(&data).unwrap());
            });
            // Exit code: 0 proved, 5 counterexample, 6 inconclusive.
            match r.status {
                spec_checker::ProofStatus::Proved => Ok(()),
                spec_checker::ProofStatus::Counterexample => std::process::exit(5),
                spec_checker::ProofStatus::Inconclusive => std::process::exit(6),
            }
        }
        "smt" => {
            let path = rest.first().ok_or_else(|| anyhow!("usage: lex spec smt <spec>"))?;
            let spec_src = read_source(path)?;
            let spec = spec_checker::parse_spec(&spec_src)
                .map_err(|e| anyhow!("spec parse: {e}"))?;
            let smt = spec_checker::to_smtlib(&spec);
            let data = serde_json::json!({ "smt_lib": &smt });
            acli::emit_or_text("spec", data, fmt, || print!("{smt}"));
            Ok(())
        }
        other => bail!("unknown `lex spec` subcommand: {other}"),
    }
}

// ---- agent-tool ----------------------------------------------------
//
// Pitch: hand an LLM a request, ask it to emit a Lex tool body, run
// the body under a declared effect set. The type checker rejects any
// body that touches effects outside that set — *before* a single byte
// runs. Lex's effect system + capability gate as a sandbox for
// agent-generated code.
//
//   lex agent-tool --allow-effects net --request "weather in Paris"
//   lex agent-tool --allow-effects net --body 'match net.get("https://wttr.in/Paris?format=3") { Ok(s) => s, Err(e) => e }'

struct AgentToolOpts {
    allowed_effects: Vec<String>,
    user_input: String,
    body_source: BodySource,
    api_key: Option<String>,
    model: String,
    show_source: bool,
    /// Cap on opcode dispatches before the VM aborts with
    /// `step limit exceeded`. Defends against agent-emitted DoS
    /// (`list.fold(list.range(0, 1e9), …)`). Default 1_000_000 —
    /// generous enough for analytics + linreg, tight enough that
    /// runaway loops surface in <1s.
    max_steps: u64,
    /// Per-path scope on `[fs_read]` / `[io]` reads. Empty = any.
    allow_fs_read: Vec<PathBuf>,
    /// Per-host scope on `[net]`. Empty = any host. Useful when a
    /// tool needs to call api.openai.com but should not be able to
    /// POST to attacker.example.com.
    allow_net_host: Vec<String>,
    /// Path to a JSON file of `[{"input": "...", "expected": "..."}, ...]`
    /// pairs. When set, the tool runs once per case and is rejected
    /// if any output mismatches `expected`. Closes the well-typed-but-
    /// wrong-behavior gap: the type system says what code touches; the
    /// examples say what it should return.
    examples_file: Option<PathBuf>,
    /// Path to a Spec file (`spec name { forall …: <bool expr> }`) to
    /// prove against the emitted body before trusting it. Counterexamples
    /// abort with exit 5 (same family as examples-failed); inconclusive
    /// proofs abort with exit 6 unless `--spec-allow-inconclusive` is
    /// set. This is the strongest behavioral check available — it lifts
    /// rung 2 from "show me the answer for these N cases" to "show me
    /// the answer for *all* cases the spec ranges over."
    spec_file: Option<PathBuf>,
    /// If true, an inconclusive Spec proof doesn't abort the run.
    /// Useful when SMT can't decide a property but you still want
    /// to ship; the spec's own evidence record stays in the trace.
    spec_allow_inconclusive: bool,
    /// Trials for randomized fallback when SMT can't decide.
    spec_trials: u32,
    /// Optional second body to compare against. When set, both bodies
    /// run on each input (single `--input` or every entry from
    /// `--examples`); any output divergence aborts with exit 7.
    /// Catches model-version regressions when v1's emission and v2's
    /// emission disagree on at least one case the host cares about.
    diff_body_source: Option<BodySource>,
    /// Store root for attestation persistence (#132). When set,
    /// every verification step (`--examples`, `--spec`, `--diff-body`,
    /// and the final sandboxed run) emits an attestation against
    /// the StageId of the agent-emitted `tool` fn. None ⇒ verifications
    /// run as before with no persistence.
    store_root: Option<PathBuf>,
}

enum BodySource {
    Request(String),
    Literal(String),
    File(PathBuf),
}

fn cmd_agent_tool(args: &[String]) -> Result<()> {
    let opts = parse_agent_tool_args(args)?;

    // 1) Get the tool body — from Claude or supplied verbatim.
    let body = match &opts.body_source {
        BodySource::Literal(b) => b.clone(),
        BodySource::File(p) => fs::read_to_string(p)
            .with_context(|| format!("read body from {}", p.display()))?,
        BodySource::Request(req) => {
            let api_key = opts.api_key.clone()
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .ok_or_else(|| anyhow!(
                    "--request needs ANTHROPIC_API_KEY (or pass --api-key); \
                     for offline use try --body or --body-file"))?;
            call_claude_for_body(req, &opts.allowed_effects, &api_key, &opts.model)?
        }
    };
    let body = strip_code_fences(&body);

    if opts.show_source {
        eprintln!("→ tool body:");
        for l in body.lines() { eprintln!("    {l}"); }
    }

    // 2) Splice into the template.
    let src = build_tool_program(&body, &opts.allowed_effects);
    if opts.show_source {
        eprintln!("→ assembled program:");
        for l in src.lines() { eprintln!("    {l}"); }
    }

    // 3) Parse + type-check. This is where a malicious body gets caught:
    // any effect not in `[allowed_effects]` shows up as an undeclared
    // effect on `fn tool` and the checker rejects it.
    let prog = load_program_from_str(&src).context("parse agent-generated source")?;
    let stages = canonicalize_program(&prog);

    // #132: every verification step below is an attestation producer
    // when `--store DIR` is set. Compute the StageId of the agent-
    // emitted `tool` fn once; open the log once. Subsequent emit
    // sites are content-addressed against this StageId so a later
    // `lex stage <id> --attestations` can answer "what evidence
    // exists for this exact body?".
    let tool_stage_id: Option<String> = stages.iter()
        .find_map(|s| match s {
            Stage::FnDecl(fd) if fd.name == "tool" => stage_id(s),
            _ => None,
        });
    let att_log: Option<lex_vcs::AttestationLog> = match &opts.store_root {
        Some(root) => {
            let store = Store::open(root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            Some(store.attestation_log()?)
        }
        None => None,
    };
    let model_for_attestation: Option<String> = match &opts.body_source {
        BodySource::Request(_) => Some(opts.model.clone()),
        _ => None,
    };

    if let Err(errs) = lex_types::check_program(&stages) {
        eprintln!("→ TYPE-CHECK REJECTED — tool not run.");
        for e in &errs {
            eprintln!("  {e}");
            if let lex_types::TypeError::EffectNotDeclared { effect, .. } = e {
                eprintln!("    (the body uses effect `{effect}` but the host only allows {:?})",
                    opts.allowed_effects);
            }
        }
        std::process::exit(2);
    }

    // 4) Compile + policy gate.
    let bc = compile_program(&stages);
    let mut policy = Policy::pure();
    policy.allow_effects = opts.allowed_effects.iter().cloned().collect();
    policy.allow_fs_read = opts.allow_fs_read.clone();
    policy.allow_net_host = opts.allow_net_host.clone();
    if let Err(violations) = check_policy(&bc, &policy) {
        eprintln!("→ POLICY REJECTED — tool not run.");
        for v in &violations {
            eprintln!("  {}", serde_json::to_string(v).unwrap_or_default());
        }
        std::process::exit(3);
    }

    // 4b) Spec proof. Strongest behavioral guarantee available pre-run:
    // a quantified property attached to `tool` is checked against the
    // emitted body before the tool ever executes on real inputs. SMT
    // (via Z3, when available) handles structural+integer cases;
    // randomized fallback covers the rest. Counterexamples abort with
    // exit 5; inconclusive aborts with 6 unless --spec-allow-inconclusive.
    if let Some(path) = opts.spec_file.as_ref() {
        let spec_text = fs::read_to_string(path)
            .with_context(|| format!("read spec file {}", path.display()))?;
        let spec = spec_checker::parse_spec(&spec_text)
            .map_err(|e| anyhow!("spec parse: {e}"))?;
        if opts.show_source {
            eprintln!("→ checking spec `{}`…", spec.name);
        }
        let report = spec_checker::check_spec(&spec, &src, opts.spec_trials);

        // Emit the Spec attestation *before* the match below acts on
        // the verdict — Counterexample / strict Inconclusive both
        // exit, so we'd lose evidence on the failure path otherwise.
        // Failures are evidence too (#132 trust model).
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let result = match &report.status {
                spec_checker::ProofStatus::Proved => lex_vcs::AttestationResult::Passed,
                spec_checker::ProofStatus::Counterexample => {
                    let detail = report.evidence.counterexample.as_ref()
                        .and_then(|c| serde_json::to_string(c).ok())
                        .map(|s| format!("counterexample: {s}"))
                        .unwrap_or_else(|| "counterexample".into());
                    lex_vcs::AttestationResult::Failed { detail }
                }
                spec_checker::ProofStatus::Inconclusive => lex_vcs::AttestationResult::Inconclusive {
                    detail: report.evidence.note.clone().unwrap_or_else(|| "inconclusive".into()),
                },
            };
            let kind = lex_vcs::AttestationKind::Spec {
                spec_id: report.spec_id.clone(),
                method: lex_vcs::SpecMethod::Random,
                trials: Some(opts.spec_trials as usize),
            };
            emit_agent_tool_attestation(log, sid, kind, result, model_for_attestation.clone())?;
        }

        match report.status {
            spec_checker::ProofStatus::Proved => {
                if opts.show_source {
                    eprintln!("  spec proved ({} method, {} trials)",
                        report.evidence.method, report.evidence.trials);
                }
            }
            spec_checker::ProofStatus::Counterexample => {
                eprintln!("→ SPEC COUNTEREXAMPLE — tool not run.");
                if let Some(cx) = &report.evidence.counterexample {
                    for (k, v) in cx { eprintln!("  {k} = {v}"); }
                }
                if let Some(note) = &report.evidence.note {
                    eprintln!("  ({note})");
                }
                std::process::exit(5);
            }
            spec_checker::ProofStatus::Inconclusive => {
                eprintln!("→ SPEC INCONCLUSIVE — could not decide property.");
                if let Some(note) = &report.evidence.note {
                    eprintln!("  ({note})");
                }
                if !opts.spec_allow_inconclusive {
                    eprintln!("  (pass --spec-allow-inconclusive to ship anyway)");
                    std::process::exit(6);
                }
                eprintln!("  (continuing because --spec-allow-inconclusive is set)");
            }
        }
    }

    // 5) Run with a step-limit cap. This is the runtime DoS guard:
    // type-check rejects effects, max_steps rejects runaway compute.
    let bc = std::sync::Arc::new(bc);

    // 5-diff) Differential evaluation: if --diff-body is set, compile
    // the second body through the same gates and run both on each input
    // (single --input or every entry from --examples). Any output
    // divergence aborts with exit 7. Use case: detect regressions when
    // model v2's emission disagrees with v1's on inputs the host cares
    // about, without needing a full Spec proof.
    if let Some(diff_src) = opts.diff_body_source.as_ref() {
        let diff_body_text = match diff_src {
            BodySource::Literal(b) => b.clone(),
            BodySource::File(p) => fs::read_to_string(p)
                .with_context(|| format!("read diff body from {}", p.display()))?,
            BodySource::Request(_) => bail!(
                "--diff-body and --diff-body-file accept literal source; \
                 invoke Claude separately and pass the body in"),
        };
        let diff_body_text = strip_code_fences(&diff_body_text);
        let diff_src = build_tool_program(&diff_body_text, &opts.allowed_effects);
        let prog_b = load_program_from_str(&diff_src).context("parse diff body")?;
        let stages_b = canonicalize_program(&prog_b);
        if let Err(errs) = lex_types::check_program(&stages_b) {
            eprintln!("→ DIFF BODY type-check rejected.");
            for e in &errs { eprintln!("  {e}"); }
            std::process::exit(2);
        }
        let bc_b = compile_program(&stages_b);
        if let Err(violations) = check_policy(&bc_b, &policy) {
            eprintln!("→ DIFF BODY policy rejected.");
            for v in &violations {
                eprintln!("  {}", serde_json::to_string(v).unwrap_or_default());
            }
            std::process::exit(3);
        }
        let bc_b = std::sync::Arc::new(bc_b);

        // Inputs: --examples list or single --input.
        let inputs: Vec<String> = match opts.examples_file.as_ref() {
            Some(p) => load_examples(p)?.into_iter().map(|e| e.input).collect(),
            None => vec![opts.user_input.clone()],
        };

        if opts.show_source {
            eprintln!("→ comparing two bodies on {} input(s)…", inputs.len());
        }
        let mut diverged: Vec<(String, String, String)> = Vec::new();
        for input in &inputs {
            let out_a = run_tool_once(&bc, &policy, opts.max_steps, input)?;
            let out_b = run_tool_once(&bc_b, &policy, opts.max_steps, input)?;
            if out_a != out_b {
                diverged.push((input.clone(), out_a, out_b));
            }
        }
        // Emit a DiffBody attestation against the original tool's
        // StageId. `other_body_hash` is the SHA-256 of the second
        // body's source so re-running with the same pair dedups.
        // Failed attestation carries a summary of how many inputs
        // diverged.
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let other_body_hash = sha256_hex(diff_body_text.as_bytes());
            let result = if diverged.is_empty() {
                lex_vcs::AttestationResult::Passed
            } else {
                lex_vcs::AttestationResult::Failed {
                    detail: format!("{}/{} inputs diverged", diverged.len(), inputs.len()),
                }
            };
            let kind = lex_vcs::AttestationKind::DiffBody {
                other_body_hash,
                input_count: inputs.len(),
            };
            emit_agent_tool_attestation(log, sid, kind, result, model_for_attestation.clone())?;
        }

        if !diverged.is_empty() {
            eprintln!("→ DIFFERENTIAL DIVERGENCE — {} of {} inputs differ.",
                diverged.len(), inputs.len());
            for (input, a, b) in &diverged {
                eprintln!("  input={input:?}");
                eprintln!("    body A → {a:?}");
                eprintln!("    body B → {b:?}");
            }
            std::process::exit(7);
        }
        if opts.show_source {
            eprintln!("→ no divergence on {} input(s)", inputs.len());
        }
        // Print body A's output on the first input — single-shot mode.
        let chosen = inputs.first().cloned().unwrap_or_default();
        let out = run_tool_once(&bc, &policy, opts.max_steps, &chosen)?;
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let kind = lex_vcs::AttestationKind::SandboxRun {
                effects: opts.allowed_effects.iter().cloned().collect(),
            };
            emit_agent_tool_attestation(
                log,
                sid,
                kind,
                lex_vcs::AttestationResult::Passed,
                model_for_attestation.clone(),
            )?;
        }
        println!("{out}");
        return Ok(());
    }

    // 5a) If --examples is set, behavioral-verify before trusting the tool
    // for live traffic. Catches the well-typed-but-wrong-behavior gap:
    // the type system says what code touches; the examples say what it
    // should return. On any mismatch, exit 5 (distinct from 2/3/4).
    if let Some(path) = opts.examples_file.as_ref() {
        let raw_examples = fs::read(path)
            .with_context(|| format!("read examples file {}", path.display()))?;
        let examples_file_hash = sha256_hex(&raw_examples);
        let examples: Vec<Example> = serde_json::from_slice(&raw_examples)
            .with_context(|| format!("parse examples file {}; expected JSON array of {{input, expected}}", path.display()))?;
        if opts.show_source {
            eprintln!("→ checking {} example(s)…", examples.len());
        }
        let mut failures: Vec<(usize, &Example, String)> = Vec::new();
        for (idx, ex) in examples.iter().enumerate() {
            let out = run_tool_once(&bc, &policy, opts.max_steps, &ex.input)?;
            if out != ex.expected {
                failures.push((idx, ex, out));
            }
        }

        // Emit Examples attestation regardless of pass/fail. Same
        // "failures are evidence too" rule as Spec.
        if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
            let result = if failures.is_empty() {
                lex_vcs::AttestationResult::Passed
            } else {
                lex_vcs::AttestationResult::Failed {
                    detail: format!("{}/{} examples mismatched", failures.len(), examples.len()),
                }
            };
            let kind = lex_vcs::AttestationKind::Examples {
                file_hash: examples_file_hash,
                count: examples.len(),
            };
            emit_agent_tool_attestation(log, sid, kind, result, model_for_attestation.clone())?;
        }

        if !failures.is_empty() {
            eprintln!("→ EXAMPLES FAILED — tool not trusted ({} of {} mismatched).",
                failures.len(), examples.len());
            for (i, ex, got) in &failures {
                eprintln!("  [{i}] input={:?}", ex.input);
                eprintln!("       expected={:?}", ex.expected);
                eprintln!("       got     ={got:?}");
            }
            std::process::exit(5);
        }
        if opts.show_source {
            eprintln!("→ examples passed: {}/{}", examples.len(), examples.len());
        }
    }

    // 5b) Single-shot run with the user_input. With --examples this
    // doubles as a sanity invocation; without examples it's the only run.
    let result = run_tool_once(&bc, &policy, opts.max_steps, &opts.user_input)?;

    // Emit a SandboxRun attestation tagging the effects the policy
    // actually allowed. `Passed` only — a runtime-error path
    // returns Err above and never reaches this point.
    if let (Some(log), Some(sid)) = (&att_log, &tool_stage_id) {
        let kind = lex_vcs::AttestationKind::SandboxRun {
            effects: opts.allowed_effects.iter().cloned().collect(),
        };
        emit_agent_tool_attestation(
            log,
            sid,
            kind,
            lex_vcs::AttestationResult::Passed,
            model_for_attestation.clone(),
        )?;
    }

    println!("{result}");
    Ok(())
}

#[derive(serde::Deserialize)]
struct Example {
    input: String,
    expected: String,
}

fn load_examples(path: &std::path::Path) -> Result<Vec<Example>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read examples file {}", path.display()))?;
    let cases: Vec<Example> = serde_json::from_str(&raw)
        .with_context(|| format!("parse examples file {}; expected JSON array of {{input, expected}}", path.display()))?;
    Ok(cases)
}

fn run_tool_once(
    bc: &std::sync::Arc<lex_bytecode::Program>,
    policy: &Policy,
    max_steps: u64,
    input: &str,
) -> Result<String> {
    let handler = DefaultHandler::new(policy.clone()).with_program(std::sync::Arc::clone(bc));
    let mut vm = Vm::with_handler(bc, Box::new(handler));
    vm.set_step_limit(max_steps);
    let result = match vm.call("tool", vec![Value::Str(input.to_string().into())]) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("step limit") {
                eprintln!("→ STEP-LIMIT EXCEEDED — tool aborted at {max_steps} steps.");
                eprintln!("  (raise with --max-steps; default {})", default_max_steps());
                std::process::exit(4);
            }
            // Runtime scope rejections (--allow-fs-read / --allow-net-host
            // / --allow-fs-write) surface as effect-handler errors. Exit 3
            // matches the static-policy gate so callers can branch cleanly:
            //   2 = type-check, 3 = policy (static or runtime), 4 = step-limit,
            //   5 = examples failed.
            if msg.contains("outside --allow-fs-read")
                || msg.contains("outside --allow-fs-write")
                || msg.contains("not in --allow-net-host")
            {
                eprintln!("→ POLICY REJECTED (runtime scope) — tool aborted.");
                eprintln!("  {e}");
                std::process::exit(3);
            }
            return Err(anyhow!("runtime: {e}"));
        }
    };
    Ok(match result {
        Value::Str(s) => s.to_string(),
        other => value_to_json_string(&other),
    })
}

const fn default_max_steps() -> u64 { 1_000_000 }

fn parse_agent_tool_args(args: &[String]) -> Result<AgentToolOpts> {
    let mut allowed_effects: Vec<String> = Vec::new();
    let mut user_input: Option<String> = None;
    let mut body: Option<BodySource> = None;
    let mut api_key: Option<String> = None;
    let mut model = "claude-sonnet-4-6".to_string();
    let mut show_source = true;
    let mut max_steps: u64 = default_max_steps();
    let mut allow_fs_read: Vec<PathBuf> = Vec::new();
    let mut allow_net_host: Vec<String> = Vec::new();
    let mut examples_file: Option<PathBuf> = None;
    let mut spec_file: Option<PathBuf> = None;
    let mut spec_allow_inconclusive = false;
    let mut spec_trials: u32 = 1000;
    let mut diff_body: Option<BodySource> = None;
    let mut store_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--allow-effects" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                allowed_effects = v.split(',').filter(|s| !s.is_empty()).map(String::from).collect();
                i += 2;
            }
            "--allow-fs-read" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--allow-fs-read needs a path"))?;
                allow_fs_read.push(PathBuf::from(v));
                i += 2;
            }
            "--allow-net-host" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--allow-net-host needs a host"))?;
                allow_net_host.push(v.clone());
                i += 2;
            }
            "--input" => {
                user_input = Some(args.get(i + 1).ok_or_else(|| anyhow!("--input needs a value"))?.clone());
                i += 2;
            }
            "--request" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--request needs a value"))?.clone();
                if user_input.is_none() { user_input = Some(v.clone()); }
                body = Some(BodySource::Request(v));
                i += 2;
            }
            "--body" => {
                body = Some(BodySource::Literal(args.get(i + 1).ok_or_else(|| anyhow!("--body needs a value"))?.clone()));
                i += 2;
            }
            "--body-file" => {
                body = Some(BodySource::File(PathBuf::from(args.get(i + 1)
                    .ok_or_else(|| anyhow!("--body-file needs a path"))?)));
                i += 2;
            }
            "--api-key" => {
                api_key = Some(args.get(i + 1).ok_or_else(|| anyhow!("--api-key needs a value"))?.clone());
                i += 2;
            }
            "--model" => {
                model = args.get(i + 1).ok_or_else(|| anyhow!("--model needs a value"))?.clone();
                i += 2;
            }
            "--max-steps" => {
                max_steps = args.get(i + 1).ok_or_else(|| anyhow!("--max-steps needs a value"))?
                    .parse().context("--max-steps must be an integer")?;
                i += 2;
            }
            "--examples" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--examples needs a path"))?;
                examples_file = Some(PathBuf::from(v));
                i += 2;
            }
            "--spec" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--spec needs a path"))?;
                spec_file = Some(PathBuf::from(v));
                i += 2;
            }
            "--spec-allow-inconclusive" => { spec_allow_inconclusive = true; i += 1; }
            "--spec-trials" => {
                spec_trials = args.get(i + 1).ok_or_else(|| anyhow!("--spec-trials needs an integer"))?
                    .parse().context("--spec-trials must be a u32")?;
                i += 2;
            }
            "--diff-body" => {
                diff_body = Some(BodySource::Literal(args.get(i + 1)
                    .ok_or_else(|| anyhow!("--diff-body needs a value"))?.clone()));
                i += 2;
            }
            "--diff-body-file" => {
                diff_body = Some(BodySource::File(PathBuf::from(args.get(i + 1)
                    .ok_or_else(|| anyhow!("--diff-body-file needs a path"))?)));
                i += 2;
            }
            "--store" => {
                store_root = Some(PathBuf::from(args.get(i + 1)
                    .ok_or_else(|| anyhow!("--store needs a path"))?));
                i += 2;
            }
            "--quiet" => { show_source = false; i += 1; }
            other => bail!("unknown agent-tool flag: {other}"),
        }
    }
    Ok(AgentToolOpts {
        allowed_effects,
        user_input: user_input.unwrap_or_default(),
        body_source: body.ok_or_else(||
            anyhow!("must pass --request '<query>', --body '<src>', or --body-file <path>"))?,
        api_key,
        model,
        show_source,
        max_steps,
        allow_fs_read,
        allow_net_host,
        examples_file,
        spec_file,
        spec_allow_inconclusive,
        spec_trials,
        diff_body_source: diff_body,
        store_root,
    })
}

fn build_tool_program(body: &str, allowed_effects: &[String]) -> String {
    // Auto-import every std module so the agent can syntactically
    // reach any effect — the *signature* gates what's allowed. This
    // makes the demo land: a body using `io.read` resolves cleanly
    // to the io builtin, then the type checker rejects it with
    // "effect `io` not declared on `fn tool`" instead of a confusing
    // unknown-identifier error.
    let imports = [
        "import \"std.io\"    as io",
        "import \"std.net\"   as net",
        "import \"std.str\"   as str",
        "import \"std.int\"   as int",
        "import \"std.float\" as float",
        "import \"std.list\"  as list",
        "import \"std.json\"  as json",
        "import \"std.bytes\" as bytes",
    ].join("\n");
    let effects = if allowed_effects.is_empty() {
        String::new()
    } else {
        format!("[{}] ", allowed_effects.join(", "))
    };
    // The tool's signature is fixed: input -> Str. The agent provides
    // only the body. Effects are declared from the host's allow-list
    // so any extra effect inside the body is an undeclared use.
    format!("{imports}\n\nfn tool(input :: Str) -> {effects}Str {{\n{body}\n}}\n")
}

fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("```lex").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    // If the model wrapped the body in `fn tool(...) { ... }`, peel it down
    // to just the inner block so the template re-wraps it cleanly.
    if let Some((_, rest)) = t.split_once("fn tool(") {
        if let Some(open) = rest.find('{') {
            let after_brace = &rest[open + 1..];
            if let Some(close) = after_brace.rfind('}') {
                return after_brace[..close].trim().to_string();
            }
        }
    }
    t.to_string()
}

fn call_claude_for_body(
    user_request: &str,
    allowed_effects: &[String],
    api_key: &str,
    model: &str,
) -> Result<String> {
    let effects_str = if allowed_effects.is_empty() {
        "(none)".to_string()
    } else {
        format!("[{}]", allowed_effects.join(", "))
    };
    let system = format!(r#"You are a code generator for the Lex programming language.

Output ONLY the body of:

    fn tool(input :: Str) -> {effects_str} Str {{ <YOUR BODY> }}

Imports already in scope: net, str, int, float, list, json.
Useful builtins:
  net.get(url :: Str) -> [net] Result[Str, Str]
  net.post(url, body) -> [net] Result[Str, Str]
  str.concat(a, b) -> Str          # use repeatedly to build a string
  str.split(s, sep) -> List[Str]
  str.contains(s, needle) -> Bool
  int.to_str(n :: Int) -> Str
  json.stringify(v) -> Str
  json.parse(s) -> Result[T, Str]

Hard constraints:
1. Only use effects from the set {effects_str}. ANY other effect (io.read,
   io.write, fs_read, fs_write, ...) will be rejected by the type
   checker before execution.
2. Output a single block-bodied expression (no `fn` declaration, no
   imports, no markdown fences). Begin directly with code.
3. Match Result with Ok/Err arms; never use a `.unwrap`.
4. Lex has no string interpolation — chain `str.concat(a, b)` calls.
"#);
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "system": system,
        "messages": [{ "role": "user", "content": user_request }],
    });
    let resp: serde_json::Value = ureq::post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send_json(body)
        .map_err(|e| anyhow!("claude api: {e}"))?
        .body_mut()
        .read_json::<serde_json::Value>()
        .context("decode claude response")?;
    let text = resp.get("content")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.iter().find_map(|item| {
            if item.get("type")?.as_str()? == "text" {
                item.get("text")?.as_str().map(String::from)
            } else { None }
        }))
        .ok_or_else(|| anyhow!("claude response missing text content; got: {resp}"))?;
    Ok(text)
}
