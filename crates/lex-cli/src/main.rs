//! Lex CLI per spec §12.1. M4-level subset: parse, check, run.
//!
//! Usage:
//!   lex parse <file>
//!   lex check <file>
//!   lex run <file> <fn> [<arg>...]    # args parsed as JSON
//!   lex hash <file>                    # canonical AST hash for each stage

use anyhow::{anyhow, bail, Context, Result};
use lex_ast::{canonicalize_program, stage_canonical_hash_hex, stage_id, Stage};
use lex_bytecode::{compile_program, Value, Vm};
use lex_syntax::parse_source;
use std::fs;
use std::io::Read;

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
        "help" | "--help" | "-h" => { print_usage(); Ok(()) }
        other => bail!("unknown command `{other}`. try `lex help`"),
    }
}

fn print_usage() {
    println!("lex — Lex toolchain (M0–M4 subset)\n");
    println!("commands:");
    println!("  parse <file>              print canonical AST as JSON");
    println!("  check <file>              type-check; exit 0 or print errors");
    println!("  run <file> <fn> [args]    execute fn (args parsed as JSON)");
    println!("  hash <file>               print stage canonical hashes");
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
    let path = args.first().ok_or_else(|| anyhow!("usage: lex run <file> <fn> [args]"))?;
    let func = args.get(1).ok_or_else(|| anyhow!("missing function name"))?;
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
    let mut vm = Vm::new(&bc);
    let vargs: Vec<Value> = args[2..]
        .iter()
        .map(|a| {
            let v: serde_json::Value = serde_json::from_str(a)
                .with_context(|| format!("arg `{a}` must be JSON"))?;
            Ok(json_to_value(&v))
        })
        .collect::<Result<Vec<_>>>()?;
    let r = vm.call(func, vargs).map_err(|e| anyhow!("runtime: {e}"))?;
    println!("{}", value_to_json_string(&r));
    Ok(())
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
    }
}
