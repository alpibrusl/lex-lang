//! `lex branch` and `lex store-merge` — snapshot branches in
//! `lex-store`, plus a three-way merge command that operates on
//! store contents (one stage per SigId, picked by HEAD map).
//!
//! Tier-1 of agent-native version control. Builds on `lex-store`'s
//! existing content-addressed substrate. See `lex-store/src/branches.rs`
//! for the data model + deferred items list.

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

pub fn cmd_branch(args: &[String]) -> Result<()> {
    let mut iter = args.iter().cloned();
    let sub = iter.next().ok_or_else(|| anyhow!(
        "usage: lex branch <list|show|create|delete|use|current> [--store DIR] ..."))?;
    let (store, rest) = open_store(&mut iter)?;
    match sub.as_str() {
        "list"    => list(&store),
        "show"    => show(&store, &rest),
        "create"  => create(&store, &rest),
        "delete"  => delete(&store, &rest),
        "use"     => use_branch(&store, &rest),
        "current" => current(&store),
        other     => bail!("unknown `lex branch` subcommand `{other}`"),
    }
}

pub fn cmd_store_merge(args: &[String]) -> Result<()> {
    let mut commit = false;
    let mut json = false;
    let mut positional: Vec<String> = Vec::new();
    for a in args.iter().cloned() {
        match a.as_str() {
            "--commit" => commit = true,
            "--json"   => json = true,
            _          => positional.push(a),
        }
    }
    if positional.len() < 2 {
        bail!("usage: lex store-merge <src> <dst> [--commit] [--json] [--store DIR]");
    }
    // Allow `--store` to appear anywhere; re-parse over what we kept.
    let mut iter2 = positional.into_iter();
    let (store, rest) = open_store(&mut iter2)?;
    if rest.len() != 2 {
        bail!("usage: lex store-merge <src> <dst> [--commit] [--json] [--store DIR]");
    }
    let src = &rest[0];
    let dst = &rest[1];

    let report = store.merge(src, dst)
        .map_err(|e| anyhow!("merge {src} into {dst}: {e}"))?;

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

fn list(store: &Store) -> Result<()> {
    let branches = store.list_branches().map_err(|e| anyhow!("list: {e}"))?;
    let cur = store.current_branch();
    for name in branches {
        let marker = if name == cur { "*" } else { " " };
        println!("{marker} {name}");
    }
    Ok(())
}

fn current(store: &Store) -> Result<()> {
    println!("{}", store.current_branch());
    Ok(())
}

fn show(store: &Store, args: &[String]) -> Result<()> {
    let name = args.first().ok_or_else(|| anyhow!("usage: lex branch show <name>"))?;
    let head = store.branch_head(name)
        .map_err(|e| anyhow!("head {name}: {e}"))?;
    if head.is_empty() {
        println!("{name}: (no stages)");
        return Ok(());
    }
    println!("{name}: {} stage(s)", head.len());
    for (sig, stage) in &head {
        println!("  {sig:.16}…  →  {stage:.16}…");
    }
    Ok(())
}

fn create(store: &Store, args: &[String]) -> Result<()> {
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
    store.create_branch(&name, &from)
        .map_err(|e| anyhow!("create {name} (from {from}): {e}"))?;
    println!("→ created branch `{name}` from `{from}`");
    Ok(())
}

fn delete(store: &Store, args: &[String]) -> Result<()> {
    let name = args.first().ok_or_else(|| anyhow!("usage: lex branch delete <name>"))?;
    store.delete_branch(name)
        .map_err(|e| anyhow!("delete {name}: {e}"))?;
    println!("→ deleted branch `{name}`");
    Ok(())
}

fn use_branch(store: &Store, args: &[String]) -> Result<()> {
    let name = args.first().ok_or_else(|| anyhow!("usage: lex branch use <name>"))?;
    store.set_current_branch(name)
        .map_err(|e| anyhow!("use {name}: {e}"))?;
    println!("→ on `{name}`");
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
