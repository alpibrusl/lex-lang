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

mod acli;
mod agent_guidelines;
mod agent_tool;
mod ast_merge;
mod attest;
mod audit;
mod blame;
mod branch;
mod canonical;
mod capsule_contract;
mod ci;
mod diff;
mod doc_sync;
mod docs;
mod examples_eval;
mod fmt;
mod init;
mod lint;
mod merge;
mod op;
mod pkg;
mod plan;
mod policy;
mod repair;
mod repl;
mod replay;
mod run;
mod serve;
mod spec;
mod stage;
mod store;
mod store_root;
mod test_runner;
mod tool_registry;
mod trust;
mod watch;

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

use agent_tool::*;
use attest::*;
use blame::*;
use canonical::*;
use plan::*;
use policy::*;
use repair::*;
use replay::*;
use run::*;
use serve::*;
use spec::*;
use stage::*;
use store::*;
use store_root::*;
use trust::*;

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
    let cmd_for_err = rest_after_format
        .first()
        .cloned()
        .unwrap_or_else(|| "lex".into());
    if let Err(e) = run(&fmt, &rest_after_format) {
        acli::emit_error(
            &cmd_for_err,
            &format!("{e:#}"),
            &fmt,
            ::acli::ExitCode::GeneralError,
        );
        std::process::exit(1);
    }
}

fn run(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let cmd = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex <command> ..."))?;
    match cmd.as_str() {
        // ACLI built-ins — emit JSON envelopes via the SDK.
        "introspect" => {
            acli::build_app().handle_introspect(fmt);
            Ok(())
        }
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
        "doc-sync" => doc_sync::cmd_doc_sync(&args[1..]),
        "plan" => cmd_plan(fmt, &args[1..]),
        "repair" => cmd_repair(fmt, &args[1..]),
        "producer-trust" => cmd_producer_trust(fmt, &args[1..]),
        "canonical" => cmd_canonical(fmt, &args[1..]),
        "keygen" => cmd_keygen(fmt, &args[1..]),
        "pkg" => pkg::cmd_pkg(&args[1..]),
        "repl" => repl::cmd_repl(&args[1..]),
        "test" => test_runner::cmd_test(fmt, &args[1..]),
        "watch" => watch::cmd_watch(&args[1..]),
        "fmt" => fmt::cmd_fmt(&args[1..]),
        "init" => init::cmd_init(&args[1..]),
        "ci" => ci::cmd_ci(&args[1..]),
        "agent-guidelines" => agent_guidelines::cmd_agent_guidelines(fmt, &args[1..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
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
    println!(
        "  init [<dir>]                       scaffold a new project (lex.toml, src/, tests/, CI)"
    );
    println!("  parse <file>                       print canonical AST as JSON");
    println!("  check [--strict] <file>            type-check; --strict adds lint warnings");
    println!("  repair <op_id> [--apply --transform '<json>'] [--store DIR]");
    println!("                                     apply a typed repair to a failed op; emits a RepairAttempt");
    println!("  plan <goal> [--budget N] [--store DIR]");
    println!("                                     list repair paths for a goal, cheapest-first, within budget");
    println!(
        "  fmt [--check] <file|dir>...        format .lex files; --check exits 1 if any need it"
    );
    println!("  ci [--no-fmt] [--src <d>] [--tests <d>]");
    println!("  doc-sync [--check] [manifest]      regenerate (or verify) docsync.toml's generated doc targets");
    println!(
        "                                     run the full pipeline: pkg install, check --strict,"
    );
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
    println!("  blame <fn> [--with-evidence]       show each fn's stage history from the store");
    println!(
        "  canonical <encode|decode> <file>   encode/decode the canonical wire form of an AST"
    );
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
    println!(
        "                                     re-execute with effect overrides keyed by NodeId"
    );
    println!("  diff <run_a> <run_b>               first NodeId where two traces diverge");
    println!("  serve [--port N] [--store DIR]     start the agent API HTTP server");
    println!("  repl [--load <file>]...            interactive evaluator; --load pre-loads source");
    println!(
        "  watch <file> [check|run] [args]    re-run check or run on file save (agent inner loop)"
    );
    println!("  docs [<path>...] [--for-agent]     emit machine-readable API / workspace docs");
    println!(
        "  test [<dir>]                       run tests/test_*.lex files (calls run_all in each)"
    );
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
    println!(
        "                                     hostname / AST kind. --json for machine-readable."
    );
    println!("  audit --query \"<text>\" [--limit N] [--effect K]");
    println!("                                     semantic search over the store; --effect");
    println!("                                     post-filters the ranked list.");
    println!("  ast-diff <file_a> <file_b>        AST-native diff: added/removed/renamed/modified");
    println!(
        "                                     fns, plus body-level patches per modified body."
    );
    println!("  ast-merge <base> <ours> <theirs>  three-way structural merge; structured-JSON");
    println!(
        "                                     conflicts via --json; --output writes merged source."
    );
    println!("  branch <subcommand> ...           snapshot branches in lex-store. subcommands:");
    println!(
        "                                     list | show <name> | create <name> [--from B] |"
    );
    println!("                                     delete <name> | use <name> | current");
    println!(
        "  store-merge <src> <dst> [--commit] [--json]  three-way merge between two branches in"
    );
    println!(
        "                                     the store; conflicts as JSON. --commit applies a"
    );
    println!("                                     clean merge; refuses if any conflicts remain.");
    println!("  merge {{start|status|resolve|defer|commit}}");
    println!(
        "                                     stateful merge for agent loops (#134); persists"
    );
    println!("                                     a session under <store>/merges/<merge_id>.json");
    println!("  policy {{block-producer|unblock-producer|require-attestation|");
    println!("          unrequire-attestation|show}}");
    println!("                                     manage <store>/policy.json — negative gate on");
    println!("                                     producers (#181) and positive gate on required");
    println!("                                     attestations for branch advance (#245)");
    println!("  producer-trust recompute --tool <id> [--window N] [--store DIR]");
    println!("  producer-trust keyring [--min-trust N] [--out FILE] [--store DIR]");
    println!("                                     recompute per-tool trust, or export a capsule");
    println!(
        "                                     trusted-keys keyring of producers above a score"
    );
    println!("  op {{show|log|push|pull|repack|gc}} [--store DIR]");
    println!("                                     inspect and sync the operation log");
    println!("  log [branch]                       show the operation log for a branch (alias of `branch log`)");
    println!(
        "  agent-guidelines [--version-only]  emit the AI-agent authoring contract (idiom rules)"
    );
    println!();
    println!("policy flags (run, replay):");
    println!("  --allow-effects k1,k2,...   permit these effect kinds");
    println!("  --allow-fs-read PATH        (repeatable) permit fs_read under PATH");
    println!("  --allow-fs-write PATH       (repeatable) permit fs_write under PATH");
    println!("  --allow-approval SCOPE,...  comma-separated scopes [approval] may request");
    println!("  --budget N                  cap aggregate declared budget");
    println!(
        "  --max-steps N               cap VM opcode dispatches at N (DoS guard; 0 = unbounded)"
    );
}

fn read_source(path: &str) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading stdin")?;
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
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading stdin")?;
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
            std::io::stdin()
                .read_to_end(&mut buf)
                .context("reading stdin")?;
            buf
        } else {
            std::fs::read(path).map_err(|e| anyhow!("read {path}: {e}"))?
        };
        lex_ast::canonical_format::decode_program(&bytes).map_err(|e| anyhow!("decode {path}: {e}"))
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
    let store =
        lex_store::Store::open(default_store_root()).with_context(|| "opening default store")?;
    let meta = store
        .get_metadata(stage_id)
        .with_context(|| format!("loading metadata for stage `{stage_id}`"))?;
    verify_metadata_signature(&meta, require_signed, trusted_key)?;
    let stage = store
        .get_ast(stage_id)
        .with_context(|| format!("loading AST for stage `{stage_id}`"))?;
    Ok(vec![stage])
}

fn cmd_parse(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let path = args
        .first()
        .ok_or_else(|| anyhow!("usage: lex parse <file>"))?;
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
            "--from-canonical" => {
                from_canonical = true;
            }
            "--strict" => {
                strict = true;
            }
            other if !other.starts_with("--") => {
                if path.is_some() {
                    bail!("usage: lex check [--from-canonical] [--strict] <file>");
                }
                path = Some(other);
            }
            other => bail!("unknown flag `{other}` for `lex check`"),
        }
    }
    let path =
        path.ok_or_else(|| anyhow!("usage: lex check [--from-canonical] [--strict] <file>"))?;
    let stages = load_stages(path, from_canonical)?;

    // #306 slice 1: when checking a `.lex` source file (not a
    // pre-built canonical AST), collect each `fn` declaration's
    // source position so type errors can be reported as
    // `file:line:col` instead of bare NodeIds. Skipped under
    // `--from-canonical` since canonical bytes carry no source span.
    let positions: Option<std::collections::BTreeMap<String, lex_types::Position>> =
        if !from_canonical && path != "-" {
            std::fs::read_to_string(path).ok().and_then(|src| {
                lex_syntax::parse_source_with_positions(&src)
                    .ok()
                    .map(|(_, fn_pos)| {
                        fn_pos
                            .into_iter()
                            .map(|(name, byte)| {
                                let (line, col) = lex_types::byte_to_line_col(&src, byte);
                                (
                                    name,
                                    lex_types::Position::new(Some(path.to_string()), line, col),
                                )
                            })
                            .collect()
                    })
            })
        } else {
            None
        };

    let check_result = match &positions {
        Some(pos) => lex_types::check_program_with_positions(&stages, pos)
            .map_err(|errs| errs.into_iter().collect::<Vec<_>>()),
        None => lex_types::check_program(&stages).map_err(|errs| {
            errs.into_iter()
                .map(lex_types::PositionedError::from)
                .collect()
        }),
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
                std::fs::read_to_string(path)
                    .ok()
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
                    println!(
                        "hint: lex run {} {path} <fn> [args]",
                        suggest_grants(&summary)
                    );
                }
            });
            if !lint_warnings.is_empty() {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(errs) => {
            let arr: Vec<serde_json::Value> = errs
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect();
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
                        "fs_read" => {
                            fs_read.insert(arg_str);
                        }
                        "fs_write" => {
                            fs_write.insert(arg_str);
                        }
                        "net" => {
                            net_host.insert(arg_str);
                        }
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

// ---- #227: ed25519 keygen + signing helpers ----------------------

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
            let detail = r
                .evidence
                .counterexample
                .as_ref()
                .and_then(|c| serde_json::to_string(c).ok())
                .map(|s| format!("counterexample: {s}"))
                .unwrap_or_else(|| "counterexample".into());
            AttestationResult::Failed { detail }
        }
        spec_checker::ProofStatus::Inconclusive => AttestationResult::Inconclusive {
            detail: r
                .evidence
                .note
                .clone()
                .unwrap_or_else(|| "inconclusive".into()),
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

fn value_to_json(v: &Value) -> serde_json::Value {
    v.to_json()
}

// ---- M6: store subcommands ----

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
