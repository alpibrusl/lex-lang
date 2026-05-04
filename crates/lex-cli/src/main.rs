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
mod acli;
mod op;
mod repl;
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
        "log" => branch::cmd_log(fmt, &args[1..]),
        "op" => op::cmd_op(fmt, &args[1..]),
        "repl" => repl::cmd_repl(&args[1..]),
        "watch" => watch::cmd_watch(&args[1..]),
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
    println!("  parse <file>                       print canonical AST as JSON");
    println!("  check <file>                       type-check; exit 0 or print errors");
    println!("  run [policy] <file> <fn> [args]    execute fn (args parsed as JSON)");
    println!("  hash <file>                        print stage canonical hashes");
    println!("  publish [--store DIR] [--activate] <file>");
    println!("                                     publish each stage to the store as Draft");
    println!("  store list [--store DIR]           list SigIds in the store");
    println!("  store get [--store DIR] <stage>    print metadata + canonical AST for a StageId");
    println!("  stage <stage> [--attestations]     print stage info, or list its attestations");
    println!("  trace <run_id>                     print a saved trace tree as JSON");
    println!("  replay <run_id> <file> <fn> [args] [--override NODE=JSON]...");
    println!("                                     re-execute with effect overrides keyed by NodeId");
    println!("  diff <run_a> <run_b>               first NodeId where two traces diverge");
    println!("  serve [--port N] [--store DIR]     start the agent API HTTP server");
    println!("  conformance <dir>                  run all JSON test descriptors in <dir>");
    println!("  spec check <spec> --source <file>  check a Spec against a Lex source");
    println!("  spec smt <spec>                    emit SMT-LIB for external Z3");
    println!("  agent-tool [--allow-effects ks] (--request 'q' | --body-file F | --body 'B')");
    println!("                                     have an LLM emit a Lex tool body, run it");
    println!("                                     under the declared effects (rejected at");
    println!("                                     type-check if it tries anything else)");
    println!("  tool-registry serve [--port N]    HTTP service to register Lex tools at runtime");
    println!("                                     and invoke them via /tools/{{id}}/invoke");
    println!("  audit [paths...] [filters]        structural code search by effect / call /");
    println!("                                     hostname / AST kind. --json for machine-readable.");
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
    let path = args.first().ok_or_else(|| anyhow!("usage: lex check <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    match lex_types::check_program(&stages) {
        Ok(_) => {
            let summary = effects_summary(&stages);
            let data = serde_json::json!({
                "ok": true,
                "stages": stages.len(),
                "required_effects": summary.kinds,
                "required_fs_read": summary.fs_read,
                "required_fs_write": summary.fs_write,
                "required_net_host": summary.net_host,
            });
            acli::emit_or_text("check", data, fmt, || {
                println!("ok");
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
    let (policy, positional, trace, max_steps, dry_run) = parse_run_flags(args)?;
    let path = positional.first().ok_or_else(|| anyhow!("usage: lex run [policy] <file> <fn> [args]"))?;
    let func = positional.get(1).ok_or_else(|| anyhow!("missing function name"))?;
    if dry_run {
        let actions = vec![serde_json::json!({
            "action": "execute",
            "file": path,
            "function": func,
            "args": &positional[2..],
            "policy": {
                "allow_effects": policy.allow_effects.iter().collect::<Vec<_>>(),
                "allow_fs_read": policy.allow_fs_read.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "allow_fs_write": policy.allow_fs_write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "allow_net_host": &policy.allow_net_host,
                "budget": policy.budget,
            },
            "trace": trace,
            "max_steps": max_steps,
        })];
        acli::emit_dry_run("run", fmt,
            &format!("would call `{func}` in {path}"), actions);
    }
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
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

    if let Err(violations) = check_policy(&bc, &policy) {
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
    let handler = DefaultHandler::new(policy).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    if let Some(n) = max_steps { vm.set_step_limit(n); }
    let recorder = lex_trace::Recorder::new();
    let trace_handle = recorder.handle();
    if trace { vm.set_tracer(Box::new(recorder)); }

    let vargs: Vec<Value> = positional[2..]
        .iter()
        .map(|a| {
            let v: serde_json::Value = serde_json::from_str(a)
                .with_context(|| format!("arg `{a}` must be JSON"))?;
            Ok(json_to_value(&v))
        })
        .collect::<Result<Vec<_>>>()?;
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let result = vm.call(func, vargs);
    let ended = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let mut trace_id: Option<String> = None;
    if trace {
        let store = lex_store::Store::open(default_store_root())?;
        let (root_out, root_err) = match &result {
            Ok(v) => (Some(value_to_json(v)), None),
            Err(e) => (None, Some(format!("{e}"))),
        };
        let tree = trace_handle.finalize(func.clone(), serde_json::Value::Null,
            root_out, root_err, started, ended);
        let id = store.save_trace(&tree)?;
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

#[allow(clippy::type_complexity)]
fn parse_run_flags(args: &[String]) -> Result<(Policy, Vec<String>, bool, Option<u64>, bool)> {
    let mut policy = Policy::pure();
    let mut positional = Vec::new();
    let mut trace = false;
    let mut max_steps: Option<u64> = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--allow-effects" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-effects needs a value"))?;
                policy.allow_effects = val.split(',').filter(|s| !s.is_empty())
                    .map(|s| s.to_string()).collect::<BTreeSet<_>>();
                i += 2;
            }
            "--allow-fs-read" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-fs-read needs a value"))?;
                policy.allow_fs_read.push(PathBuf::from(val));
                i += 2;
            }
            "--allow-fs-write" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-fs-write needs a value"))?;
                policy.allow_fs_write.push(PathBuf::from(val));
                i += 2;
            }
            "--allow-net-host" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-net-host needs a value"))?;
                policy.allow_net_host.push(val.clone());
                i += 2;
            }
            "--allow-proc" => {
                // Comma-separated binary basenames the [proc] effect
                // is allowed to spawn. Read SECURITY.md before granting.
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--allow-proc needs a value"))?;
                for name in val.split(',').filter(|s| !s.is_empty()) {
                    policy.allow_proc.push(name.to_string());
                }
                i += 2;
            }
            "--budget" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--budget needs a value"))?;
                policy.budget = Some(val.parse().context("--budget must be an integer")?);
                i += 2;
            }
            "--max-steps" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--max-steps needs a value"))?;
                max_steps = Some(val.parse().context("--max-steps must be an integer")?);
                i += 2;
            }
            "--trace" => { trace = true; i += 1; }
            "--dry-run" => { dry_run = true; i += 1; }
            _ => { positional.push(a.clone()); i += 1; }
        }
    }
    Ok((policy, positional, trace, max_steps, dry_run))
}

fn cmd_trace(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let id = args.first().ok_or_else(|| anyhow!("usage: lex trace <run_id>"))?;
    let store = lex_store::Store::open(default_store_root())?;
    let tree = store.load_trace(id)?;
    let data = serde_json::to_value(&tree)?;
    acli::emit_or_text("trace", data.clone(), fmt, || {
        println!("{}", serde_json::to_string_pretty(&data).unwrap());
    });
    Ok(())
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
    // usage: lex blame [--store DIR] <file>
    let (root, rest, _, _) = parse_store_flag(args);
    let path = rest.first().ok_or_else(|| anyhow!("usage: lex blame [--store DIR] <file>"))?;
    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;

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

        entries.push(serde_json::json!({
            "name": name,
            "sig_id": sig,
            "here_stage_id": here_stage,
            "here_status": here_status,    // None => unpublished
            "active_stage_id": active_stage,
            "history": history.iter().map(|h| serde_json::json!({
                "stage_id": h.stage_id,
                "status": format!("{:?}", h.status).to_lowercase(),
                "last_at": h.last_at,
                "published_at": h.published_at,
            })).collect::<Vec<_>>(),
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
    // Pull --branch off as well.
    let mut branch: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else {
            positional.push(a.clone());
        }
    }
    let path = positional.first().ok_or_else(|| anyhow!(
        "usage: lex publish [--store DIR] [--branch NAME] [--activate] <file>"))?;

    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
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

    let outcome = store.publish_program(&branch, &stages, &report, &new_imports, activate)?;
    let data = serde_json::json!({
        "ops": outcome.ops,
        "head_op": outcome.head_op,
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
            let id = rest.first().ok_or_else(|| anyhow!("usage: lex store get <stage_id>"))?;
            let meta = store.get_metadata(id)?;
            let ast = store.get_ast(id)?;
            let v = serde_json::json!({
                "metadata": serde_json::to_value(&meta)?,
                "status": format!("{:?}", store.get_status(id)?).to_lowercase(),
                "ast": serde_json::to_value(&ast)?,
            });
            acli::emit_or_text("store", v.clone(), fmt, || {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            });
            Ok(())
        }
        other => bail!("unknown `lex store` subcommand: {other}"),
    }
}

/// `lex stage <stage_id>` — print metadata + canonical AST + status.
/// `lex stage <stage_id> --attestations` — list every attestation
/// for the stage, newest-first by timestamp. CLI mirror of
/// `GET /v1/stage/<id>` and `GET /v1/stage/<id>/attestations`.
fn cmd_stage(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest, _, _) = parse_store_flag(args);
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
        listing.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
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
            _ => i += 1,
        }
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
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--source" => { src_path = rest.get(i + 1); i += 2; }
                    "--trials" => {
                        trials = rest.get(i + 1).and_then(|s| s.parse().ok())
                            .ok_or_else(|| anyhow!("--trials needs a u32"))?;
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
        println!("{out}");
        return Ok(());
    }

    // 5a) If --examples is set, behavioral-verify before trusting the tool
    // for live traffic. Catches the well-typed-but-wrong-behavior gap:
    // the type system says what code touches; the examples say what it
    // should return. On any mismatch, exit 5 (distinct from 2/3/4).
    if let Some(path) = opts.examples_file.as_ref() {
        let examples = load_examples(path)?;
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
    let result = match vm.call("tool", vec![Value::Str(input.to_string())]) {
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
        Value::Str(s) => s,
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
