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

use anyhow::{anyhow, bail, Context, Result};
use lex_ast::{canonicalize_program, sig_id, stage_canonical_hash_hex, stage_id, Stage};
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{check_program as check_policy, DefaultHandler, Policy};
use lex_store::Store;
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    let cmd = args.first().ok_or_else(|| anyhow!("usage: lex <command> ..."))?;
    match cmd.as_str() {
        "parse" => cmd_parse(&args[1..]),
        "check" => cmd_check(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "hash" => cmd_hash(&args[1..]),
        "publish" => cmd_publish(&args[1..]),
        "store" => cmd_store(&args[1..]),
        "trace" => cmd_trace(&args[1..]),
        "replay" => cmd_replay(&args[1..]),
        "diff" => cmd_diff(&args[1..]),
        "serve" => cmd_serve(&args[1..]),
        "conformance" => cmd_conformance(&args[1..]),
        "help" | "--help" | "-h" => { print_usage(); Ok(()) }
        other => bail!("unknown command `{other}`. try `lex help`"),
    }
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
    println!("  trace <run_id>                     print a saved trace tree as JSON");
    println!("  replay <run_id> <file> <fn> [args] [--override NODE=JSON]...");
    println!("                                     re-execute with effect overrides keyed by NodeId");
    println!("  diff <run_a> <run_b>               first NodeId where two traces diverge");
    println!("  serve [--port N] [--store DIR]     start the agent API HTTP server");
    println!("  conformance <dir>                  run all JSON test descriptors in <dir>");
    println!();
    println!("policy flags (run, replay):");
    println!("  --allow-effects k1,k2,...   permit these effect kinds");
    println!("  --allow-fs-read PATH        (repeatable) permit fs_read under PATH");
    println!("  --allow-fs-write PATH       (repeatable) permit fs_write under PATH");
    println!("  --budget N                  cap aggregate declared budget");
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

fn cmd_parse(args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!("usage: lex parse <file>"))?;
    let src = read_source(path)?;
    let prog = parse_source(&src)?;
    let stages = canonicalize_program(&prog);
    let json = serde_json::to_string_pretty(&stages)?;
    println!("{json}");
    Ok(())
}

fn cmd_check(args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!("usage: lex check <file>"))?;
    let src = read_source(path)?;
    let prog = parse_source(&src)?;
    let stages = canonicalize_program(&prog);
    match lex_types::check_program(&stages) {
        Ok(_) => {
            println!("ok");
            Ok(())
        }
        Err(errs) => {
            for e in &errs {
                let json = serde_json::to_string(e)?;
                println!("{json}");
            }
            std::process::exit(2);
        }
    }
}

fn cmd_run(args: &[String]) -> Result<()> {
    let (policy, positional, trace) = parse_run_flags(args)?;
    let path = positional.first().ok_or_else(|| anyhow!("usage: lex run [policy] <file> <fn> [args]"))?;
    let func = positional.get(1).ok_or_else(|| anyhow!("missing function name"))?;
    let src = read_source(path)?;
    let prog = parse_source(&src)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        for e in &errs {
            let json = serde_json::to_string(e)?;
            eprintln!("{json}");
        }
        std::process::exit(2);
    }
    let bc = compile_program(&stages);

    if let Err(violations) = check_policy(&bc, &policy) {
        for v in &violations {
            let json = serde_json::to_string(v)?;
            eprintln!("{json}");
        }
        std::process::exit(3);
    }

    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
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
    if trace {
        let store = lex_store::Store::open(default_store_root())?;
        let (root_out, root_err) = match &result {
            Ok(v) => (Some(value_to_json(v)), None),
            Err(e) => (None, Some(format!("{e}"))),
        };
        let tree = trace_handle.finalize(func.clone(), serde_json::Value::Null,
            root_out, root_err, started, ended);
        let id = store.save_trace(&tree)?;
        eprintln!("trace saved: {id}");
    }
    let r = result.map_err(|e| anyhow!("runtime: {e}"))?;
    println!("{}", value_to_json_string(&r));
    Ok(())
}

fn parse_run_flags(args: &[String]) -> Result<(Policy, Vec<String>, bool)> {
    let mut policy = Policy::pure();
    let mut positional = Vec::new();
    let mut trace = false;
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
            "--budget" => {
                let val = args.get(i + 1).ok_or_else(|| anyhow!("--budget needs a value"))?;
                policy.budget = Some(val.parse().context("--budget must be an integer")?);
                i += 2;
            }
            "--trace" => { trace = true; i += 1; }
            _ => { positional.push(a.clone()); i += 1; }
        }
    }
    Ok((policy, positional, trace))
}

fn cmd_trace(args: &[String]) -> Result<()> {
    let id = args.first().ok_or_else(|| anyhow!("usage: lex trace <run_id>"))?;
    let store = lex_store::Store::open(default_store_root())?;
    let tree = store.load_trace(id)?;
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(())
}

fn cmd_diff(args: &[String]) -> Result<()> {
    let a = args.first().ok_or_else(|| anyhow!("usage: lex diff <run_a> <run_b>"))?;
    let b = args.get(1).ok_or_else(|| anyhow!("missing second run id"))?;
    let store = lex_store::Store::open(default_store_root())?;
    let ta = store.load_trace(a)?;
    let tb = store.load_trace(b)?;
    match lex_trace::diff_runs(&ta, &tb) {
        Some(d) => {
            println!("{}", serde_json::to_string_pretty(&d)?);
            Ok(())
        }
        None => { println!("{{\"divergence\":null}}"); Ok(()) }
    }
}

fn cmd_hash(args: &[String]) -> Result<()> {
    let path = args.first().ok_or_else(|| anyhow!("usage: lex hash <file>"))?;
    let src = read_source(path)?;
    let prog = parse_source(&src)?;
    let stages = canonicalize_program(&prog);
    for s in &stages {
        let name = stage_name(s);
        let h = stage_canonical_hash_hex(s);
        let sid = stage_id(s).unwrap_or_else(|| "-".into());
        println!("{name}\tcanonical_ast={h}\tstage_id={sid}");
    }
    Ok(())
}

fn stage_name(s: &Stage) -> &str {
    match s {
        Stage::FnDecl(fd) => &fd.name,
        Stage::TypeDecl(td) => &td.name,
        Stage::Import(i) => &i.alias,
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Unit,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() { Value::Int(i) }
            else if let Some(f) = n.as_f64() { Value::Float(f) }
            else { Value::Unit }
        }
        J::String(s) => Value::Str(s.clone()),
        J::Array(items) => Value::List(items.iter().map(json_to_value).collect()),
        J::Object(map) => {
            let mut out = indexmap::IndexMap::new();
            for (k, v) in map { out.insert(k.clone(), json_to_value(v)); }
            Value::Record(out)
        }
    }
}

fn value_to_json_string(v: &Value) -> String {
    serde_json::to_string(&value_to_json(v)).unwrap()
}

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Int(n) => J::from(*n),
        Value::Float(f) => J::from(*f),
        Value::Bool(b) => J::Bool(*b),
        Value::Str(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(b.iter().map(|b| format!("{:02x}", b)).collect()),
        Value::Unit => J::Null,
        Value::List(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Tuple(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Record(fields) => {
            let mut m = serde_json::Map::new();
            for (k, v) in fields { m.insert(k.clone(), value_to_json(v)); }
            J::Object(m)
        }
        Value::Variant { name, args } => {
            let mut m = serde_json::Map::new();
            m.insert("$variant".into(), J::String(name.clone()));
            m.insert("args".into(), J::Array(args.iter().map(value_to_json).collect()));
            J::Object(m)
        }
        Value::Closure { fn_id, .. } => J::String(format!("<closure fn_{fn_id}>")),
    }
}

// ---- M6: store subcommands ----

fn default_store_root() -> PathBuf {
    if let Ok(s) = std::env::var("LEX_STORE") { return PathBuf::from(s); }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".lex").join("store");
    }
    PathBuf::from(".lex-store")
}

fn parse_store_flag(args: &[String]) -> (PathBuf, Vec<String>, bool) {
    // Returns (store_root, remaining_args, activate).
    let mut root = default_store_root();
    let mut activate = false;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--store" => {
                if let Some(v) = args.get(i + 1) { root = PathBuf::from(v); i += 2; } else { i += 1; }
            }
            "--activate" => { activate = true; i += 1; }
            _ => { rest.push(args[i].clone()); i += 1; }
        }
    }
    (root, rest, activate)
}

fn cmd_publish(args: &[String]) -> Result<()> {
    let (root, rest, activate) = parse_store_flag(args);
    let path = rest.first().ok_or_else(|| anyhow!("usage: lex publish [--store DIR] [--activate] <file>"))?;
    let src = read_source(path)?;
    let prog = parse_source(&src)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        for e in &errs {
            eprintln!("{}", serde_json::to_string(e)?);
        }
        std::process::exit(2);
    }
    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let mut out = Vec::new();
    for s in &stages {
        // Imports aren't stages.
        if matches!(s, Stage::Import(_)) { continue; }
        let id = store.publish(s).with_context(|| "publishing stage")?;
        if activate {
            store.activate(&id).with_context(|| format!("activating {id}"))?;
        }
        let name = match s { Stage::FnDecl(fd) => &fd.name, Stage::TypeDecl(td) => &td.name, _ => "?" };
        let sig = sig_id(s).unwrap_or_else(|| "-".into());
        let entry = serde_json::json!({
            "name": name,
            "sig_id": sig,
            "stage_id": id,
            "status": format!("{:?}", store.get_status(&id)?).to_lowercase(),
        });
        out.push(entry);
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn cmd_store(args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!("usage: lex store {{list|get}} ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "list" => {
            let (root, _rest, _) = parse_store_flag(rest);
            let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
            let sigs = store.list_sigs()?;
            for s in sigs {
                let active = store.resolve_sig(&s)?.unwrap_or_default();
                println!("{s}\tactive={active}");
            }
            Ok(())
        }
        "get" => {
            let (root, rest, _) = parse_store_flag(rest);
            let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
            let id = rest.first().ok_or_else(|| anyhow!("usage: lex store get <stage_id>"))?;
            let meta = store.get_metadata(id)?;
            let ast = store.get_ast(id)?;
            let v = serde_json::json!({
                "metadata": serde_json::to_value(&meta)?,
                "status": format!("{:?}", store.get_status(id)?).to_lowercase(),
                "ast": serde_json::to_value(&ast)?,
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        other => bail!("unknown `lex store` subcommand: {other}"),
    }
}

fn cmd_replay(args: &[String]) -> Result<()> {
    // usage: lex replay <run_id> <file> <fn> [args] [--override NODE=JSON]
    // Re-runs the function with overrides keyed by NodeId. Saves a fresh
    // trace and prints its run_id. The original run_id is referenced for
    // the user's bookkeeping; we don't currently load it (the function is
    // re-executed from source).
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

    let src = read_source(path)?;
    let prog = parse_source(&src)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        for e in &errs { eprintln!("{}", serde_json::to_string(e)?); }
        std::process::exit(2);
    }
    let bc = compile_program(&stages);
    if let Err(violations) = check_policy(&bc, &policy) {
        for v in &violations { eprintln!("{}", serde_json::to_string(v)?); }
        std::process::exit(3);
    }

    let recorder = lex_trace::Recorder::new().with_overrides(overrides);
    let handle = recorder.handle();
    let handler = DefaultHandler::new(policy);
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
    eprintln!("trace saved: {new_run_id}");
    let r = result.map_err(|e| anyhow!("runtime: {e}"))?;
    println!("{}", value_to_json_string(&r));
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

fn cmd_conformance(args: &[String]) -> Result<()> {
    let dir = args.first().ok_or_else(|| anyhow!("usage: lex conformance <dir>"))?;
    let report = conformance::run_directory(dir).context("reading conformance directory")?;
    for name in &report.passed { println!("PASS  {name}"); }
    for (name, why) in &report.failed { println!("FAIL  {name}: {why}"); }
    println!();
    println!("{}/{} passed", report.passed.len(), report.total());
    if report.ok() { Ok(()) } else { std::process::exit(4); }
}
