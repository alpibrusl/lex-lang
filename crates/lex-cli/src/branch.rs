//! `lex branch` and `lex store-merge` — snapshot branches in
//! `lex-store`, plus a three-way merge command that operates on
//! store contents (one stage per SigId, picked by HEAD map).
//!
//! Tier-1 of agent-native version control. Builds on `lex-store`'s
//! existing content-addressed substrate. See `lex-store/src/branches.rs`
//! for the data model + deferred items list.

use crate::acli as acli_mod;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Result};
use lex_store::{MergeReport, Store};
use std::path::PathBuf;

fn open_store(args_iter: &mut dyn Iterator<Item = String>) -> Result<(Store, Vec<String>)> {
    // Pull off `--store DIR` if present; pass the rest back.
    let mut rest: Vec<String> = Vec::new();
    let mut root: Option<PathBuf> = None;
    while let Some(a) = args_iter.next() {
        if a == "--store" {
            root = Some(PathBuf::from(args_iter.next()
                .ok_or_else(|| anyhow!("--store needs a path"))?));
        } else {
            rest.push(a);
        }
    }
    let root = root.unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".lex/store")
    });
    let store = Store::open(&root)
        .map_err(|e| anyhow!("open store at {}: {e}", root.display()))?;
    Ok((store, rest))
}

pub fn cmd_branch(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut iter = args.iter().cloned();
    let sub = iter.next().ok_or_else(|| anyhow!(
        "usage: lex branch <list|show|create|delete|use|current> [--store DIR] ..."))?;
    // Detect a trailing --dry-run shared by state-modifying subcommands.
    let raw: Vec<String> = iter.collect();
    let dry_run = raw.iter().any(|a| a == "--dry-run");
    let mut filtered: std::vec::IntoIter<String> = raw.into_iter()
        .filter(|a| a != "--dry-run").collect::<Vec<_>>().into_iter();
    let (store, rest) = open_store(&mut filtered)?;
    match sub.as_str() {
        "list"    => list(fmt, &store),
        "show"    => show(fmt, &store, &rest),
        "create"  => create(fmt, &store, &rest, dry_run),
        "delete"  => delete(fmt, &store, &rest, dry_run),
        "use"     => use_branch(fmt, &store, &rest, dry_run),
        "current" => current(fmt, &store),
        other     => bail!("unknown `lex branch` subcommand `{other}`"),
    }
}

pub fn cmd_store_merge(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut commit = false;
    // `--json` is the legacy flag; `--output json` from the parent
    // dispatcher is the spec-compliant equivalent — honor either.
    let mut json = matches!(fmt, OutputFormat::Json);
    let mut dry_run = false;
    let mut positional: Vec<String> = Vec::new();
    for a in args.iter().cloned() {
        match a.as_str() {
            "--commit"  => commit = true,
            "--json"    => json = true,
            "--dry-run" => dry_run = true,
            _           => positional.push(a),
        }
    }
    if positional.len() < 2 {
        bail!("usage: lex store-merge <src> <dst> [--commit] [--json] [--store DIR]");
    }
    let mut iter2 = positional.into_iter();
    let (store, rest) = open_store(&mut iter2)?;
    if rest.len() != 2 {
        bail!("usage: lex store-merge <src> <dst> [--commit] [--json] [--store DIR]");
    }
    let src = &rest[0];
    let dst = &rest[1];

    let report = store.merge(src, dst)
        .map_err(|e| anyhow!("merge {src} into {dst}: {e}"))?;

    if dry_run && commit {
        let action = serde_json::json!({
            "action": "commit-merge",
            "src": src,
            "dst": dst,
            "merged": report.merged.len(),
            "conflicts": report.conflicts.len(),
        });
        acli_mod::emit_dry_run("store-merge", fmt,
            &format!("would commit merge of `{src}` into `{dst}`"), vec![action]);
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text_report(&report, src, dst);
    }

    if !report.conflicts.is_empty() {
        std::process::exit(2);
    }
    if commit {
        store.commit_merge(dst, &report)
            .map_err(|e| anyhow!("commit merge into {dst}: {e}"))?;
        if !json {
            eprintln!("→ committed merge into `{dst}` ({} fns)", report.merged.len());
        }
    }
    Ok(())
}

// ---- branch subcommands -------------------------------------------

fn list(fmt: &OutputFormat, store: &Store) -> Result<()> {
    let branches = store.list_branches().map_err(|e| anyhow!("list: {e}"))?;
    let cur = store.current_branch();
    let entries: Vec<serde_json::Value> = branches.iter()
        .map(|n| serde_json::json!({ "name": n, "current": *n == cur }))
        .collect();
    let data = serde_json::json!({ "branches": entries, "current": cur });
    acli_mod::emit_or_text("branch", data, fmt, || {
        for name in &branches {
            let marker = if *name == cur { "*" } else { " " };
            println!("{marker} {name}");
        }
    });
    Ok(())
}

fn current(fmt: &OutputFormat, store: &Store) -> Result<()> {
    let cur = store.current_branch();
    let data = serde_json::json!({ "current": &cur });
    acli_mod::emit_or_text("branch", data, fmt, || println!("{cur}"));
    Ok(())
}

fn show(fmt: &OutputFormat, store: &Store, args: &[String]) -> Result<()> {
    let name = args.first().ok_or_else(|| anyhow!("usage: lex branch show <name>"))?;
    let head = store.branch_head(name)
        .map_err(|e| anyhow!("head {name}: {e}"))?;
    let data = serde_json::json!({
        "name": name,
        "stage_count": head.len(),
        "head": serde_json::to_value(&head)?,
    });
    acli_mod::emit_or_text("branch", data, fmt, || {
        if head.is_empty() {
            println!("{name}: (no stages)");
        } else {
            println!("{name}: {} stage(s)", head.len());
            for (sig, stage) in &head {
                println!("  {sig:.16}…  →  {stage:.16}…");
            }
        }
    });
    Ok(())
}

fn create(fmt: &OutputFormat, store: &Store, args: &[String], dry_run: bool) -> Result<()> {
    let mut name: Option<String> = None;
    let mut from = lex_store::DEFAULT_BRANCH.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                from = args.get(i + 1).ok_or_else(|| anyhow!("--from needs a branch"))?.clone();
                i += 2;
            }
            other if name.is_none() => { name = Some(other.into()); i += 1; }
            other => bail!("unexpected `{other}`"),
        }
    }
    let name = name.ok_or_else(|| anyhow!("usage: lex branch create <name> [--from BRANCH]"))?;
    if dry_run {
        let action = serde_json::json!({ "action": "create-branch", "name": name, "from": from });
        acli_mod::emit_dry_run("branch", fmt,
            &format!("would create `{name}` from `{from}`"), vec![action]);
    }
    store.create_branch(&name, &from)
        .map_err(|e| anyhow!("create {name} (from {from}): {e}"))?;
    let data = serde_json::json!({ "created": &name, "from": &from });
    acli_mod::emit_or_text("branch", data, fmt, || {
        println!("→ created branch `{name}` from `{from}`");
    });
    Ok(())
}

fn delete(fmt: &OutputFormat, store: &Store, args: &[String], dry_run: bool) -> Result<()> {
    let name = args.first().ok_or_else(|| anyhow!("usage: lex branch delete <name>"))?;
    if dry_run {
        let action = serde_json::json!({ "action": "delete-branch", "name": name });
        acli_mod::emit_dry_run("branch", fmt,
            &format!("would delete `{name}`"), vec![action]);
    }
    store.delete_branch(name)
        .map_err(|e| anyhow!("delete {name}: {e}"))?;
    let data = serde_json::json!({ "deleted": name });
    acli_mod::emit_or_text("branch", data, fmt, || {
        println!("→ deleted branch `{name}`");
    });
    Ok(())
}

fn use_branch(fmt: &OutputFormat, store: &Store, args: &[String], dry_run: bool) -> Result<()> {
    let name = args.first().ok_or_else(|| anyhow!("usage: lex branch use <name>"))?;
    if dry_run {
        let action = serde_json::json!({ "action": "use-branch", "name": name });
        acli_mod::emit_dry_run("branch", fmt,
            &format!("would switch to `{name}`"), vec![action]);
    }
    store.set_current_branch(name)
        .map_err(|e| anyhow!("use {name}: {e}"))?;
    let data = serde_json::json!({ "current": name });
    acli_mod::emit_or_text("branch", data, fmt, || {
        println!("→ on `{name}`");
    });
    Ok(())
}

// ---- pretty rendering ---------------------------------------------

fn print_text_report(report: &MergeReport, src: &str, dst: &str) {
    let base = report.summary.base.as_deref().unwrap_or("(none)");
    println!("merging {src} → {dst}  (common ancestor: {base})");
    if report.conflicts.is_empty() {
        println!("→ clean merge: {} stage(s)", report.summary.clean);
    } else {
        println!("→ {} conflict(s), {} clean", report.summary.conflicts,
            report.summary.clean);
    }
    for m in &report.merged {
        println!("  ✓ {:<11} {:.16}…  →  {:.16}…", m.from, m.sig_id, m.stage_id);
    }
    for c in &report.conflicts {
        println!("  ✗ {:<13} sig={:.16}…", c.kind, c.sig_id);
        if let Some(b) = &c.base   { println!("      base:   {b:.16}…"); }
        if let Some(s) = &c.src    { println!("      src:    {s:.16}…"); }
        if let Some(d) = &c.dst    { println!("      dst:    {d:.16}…"); }
    }
}
