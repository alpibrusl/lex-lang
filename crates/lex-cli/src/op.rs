//! `lex op show` and `lex op log`.

use crate::acli;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Result};
use lex_store::Store;
use lex_vcs::{OpLog, OperationRecord};
use std::path::PathBuf;

fn parse_store(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--store" {
            if let Some(p) = it.next() { root = Some(PathBuf::from(p)); }
        } else {
            rest.push(a.clone());
        }
    }
    let root = root.unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".lex/store")
    });
    (root, rest)
}

pub fn cmd_op(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex op {{show|log}} [--store DIR] ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "show" => cmd_op_show(fmt, rest),
        "log"  => cmd_op_log(fmt, rest),
        other  => bail!("unknown `lex op` subcommand: {other}"),
    }
}

fn cmd_op_show(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let op_id = rest.first().ok_or_else(|| anyhow!(
        "usage: lex op show [--store DIR] <op_id>"))?;
    let log = OpLog::open(&root)?;
    let rec = log.get(op_id)?
        .ok_or_else(|| anyhow!("unknown op_id: {op_id}"))?;
    let data = serde_json::json!({ "op": serde_json::to_value(&rec)? });
    acli::emit_or_text("op", data, fmt, || render_record(&rec));
    Ok(())
}

fn cmd_op_log(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut branch: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else if a == "--limit" {
            limit = Some(it.next().ok_or_else(|| anyhow!("--limit needs N"))?
                .parse().map_err(|e| anyhow!("--limit: {e}"))?);
        }
    }
    let store = Store::open(&root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());
    let head = store.get_branch(&branch)?
        .and_then(|b| b.head_op);
    let log = OpLog::open(&root)?;
    let recs = match head {
        Some(h) => log.walk_back(&h, limit)?,
        None => Vec::new(),
    };
    let arr: Vec<serde_json::Value> = recs.iter()
        .map(|r| serde_json::to_value(r).unwrap()).collect();
    let data = serde_json::json!({ "log": arr, "branch": branch });
    acli::emit_or_text("op", data, fmt, || {
        for r in &recs { render_record(r); }
    });
    Ok(())
}

fn render_record(r: &OperationRecord) {
    println!("op_id:   {}", r.op_id);
    let kind_label = serde_json::to_value(&r.op.kind).ok()
        .and_then(|v| v.get("op").and_then(|s| s.as_str().map(str::to_string)))
        .unwrap_or_else(|| "?".into());
    println!("kind:    {kind_label}");
    if r.op.parents.is_empty() {
        println!("parents: (none)");
    } else {
        for p in &r.op.parents {
            println!("parent:  {p}");
        }
    }
    println!();
}
