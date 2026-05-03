//! `lex ast-merge` — three-way structural merge of two divergent
//! Lex sources against a common base.
//!
//! Pitch (from the agent-native VC sketch the user shared): when two
//! agents disagree, you get *structured JSON*, not corrupted source
//! with `<<<<<<< HEAD` markers. The receiving agent reads the
//! conflict, resolves the logic programmatically, submits the unified
//! AST. No re-parse of broken text.
//!
//! V1 scope: top-level FnDecl-by-name three-way merge. Body-level
//! merge (overlapping changes inside a fn) is conservative — if the
//! same fn was modified on both sides, it's flagged as a conflict
//! and both bodies are returned in the JSON. Refining body-level
//! patches into structural-merge is a follow-up.

use crate::acli as acli_mod;
use ::acli::OutputFormat;
use anyhow::{anyhow, Context, Result};
use lex_ast::{
    canon_print::print_stages, canonicalize_program, FnDecl, Stage,
    stage_canonical_hash_hex,
};
use lex_syntax::load_program;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

#[derive(Default)]
struct MergeOpts {
    files: Vec<PathBuf>, // [base, ours, theirs]
    json: bool,
    /// Where to write the merged source. Renamed from `--output` to
    /// `--write` to avoid colliding with ACLI's `--output FORMAT`.
    /// Old `--output PATH` still accepted for backwards compat as
    /// long as PATH isn't a known format keyword (text/json/table).
    output: Option<PathBuf>,
    dry_run: bool,
}

#[derive(Serialize)]
struct MergedFn {
    name: String,
    /// Where the merged version came from: "base" (unchanged on
    /// both sides), "ours", "theirs", "both" (both sides made the
    /// same edit), or "added-ours" / "added-theirs" / "added-both".
    from: &'static str,
}

#[derive(Serialize)]
struct Conflict {
    /// The conflict kind; lets agent code branch cleanly.
    /// One of: "modify-modify", "modify-delete", "delete-modify",
    /// "add-add".
    kind: &'static str,
    name: String,
    /// The function as it was on the merge base. None for add-add.
    base: Option<String>,
    /// Ours / theirs are the function pretty-printed as Lex source.
    /// Agents resolving the conflict typically want the source they
    /// can re-parse, not just an AST-JSON blob.
    ours:   Option<String>,
    theirs: Option<String>,
}

#[derive(Serialize)]
struct MergeReport {
    summary: MergeSummary,
    merged: Vec<MergedFn>,
    conflicts: Vec<Conflict>,
}

#[derive(Serialize, Default)]
struct MergeSummary {
    total_fns: usize,
    clean: usize,
    conflicts: usize,
}

pub fn cmd_ast_merge(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let opts = parse_args(args)?;
    if opts.files.len() != 3 {
        return Err(anyhow!(
            "usage: lex ast-merge <base.lex> <ours.lex> <theirs.lex> \
             [--json] [--write PATH] [--dry-run]"));
    }
    let base   = load_fns(&opts.files[0])?;
    let ours   = load_fns(&opts.files[1])?;
    let theirs = load_fns(&opts.files[2])?;
    let report = compute_merge(&base, &ours, &theirs);
    let json = opts.json || matches!(fmt, OutputFormat::Json);

    if opts.dry_run {
        let action = serde_json::json!({
            "action": "merge",
            "base": opts.files[0].display().to_string(),
            "ours": opts.files[1].display().to_string(),
            "theirs": opts.files[2].display().to_string(),
            "would_write": opts.output.as_ref().map(|p| p.display().to_string()),
            "merged": report.merged.len(),
            "conflicts": report.conflicts.len(),
        });
        acli_mod::emit_dry_run("ast-merge", fmt, "would compute three-way merge",
            vec![action]);
    }

    if let Some(out) = &opts.output {
        if !report.conflicts.is_empty() {
            return Err(anyhow!(
                "{} conflicts; refusing to write merged source. \
                 Re-run without --write to see structured JSON.",
                report.conflicts.len()));
        }
        let stages: Vec<Stage> = report.merged.iter()
            .map(|m| pick_fn(&m.name, &base, &ours, &theirs, m.from))
            .map(Stage::FnDecl)
            .collect();
        let src = print_stages(&stages);
        fs::write(out, src).with_context(|| format!("write {}", out.display()))?;
        if !json {
            eprintln!("→ wrote merged source to {} ({} fns)",
                out.display(), report.merged.len());
        }
    }

    if matches!(fmt, OutputFormat::Json) {
        let data = serde_json::to_value(&report)?;
        acli_mod::emit_or_text("ast-merge", data, fmt, || {});
    } else if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if report.conflicts.is_empty() {
            println!("→ clean merge: {} fn(s)", report.summary.clean);
        } else {
            println!("→ {} conflict(s), {} clean merge(s)",
                report.summary.conflicts, report.summary.clean);
        }
        for m in &report.merged {
            println!("  ✓ {:<10} {}", m.from, m.name);
        }
        for c in &report.conflicts {
            println!("  ✗ {:<10} {} ({})", c.kind, c.name,
                short_kind_message(c.kind));
        }
    }
    if !report.conflicts.is_empty() {
        std::process::exit(2);
    }
    Ok(())
}

fn parse_args(args: &[String]) -> Result<MergeOpts> {
    let mut o = MergeOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json"    => { o.json = true; i += 1; }
            "--dry-run" => { o.dry_run = true; i += 1; }
            "--write" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--write needs a path"))?;
                o.output = Some(PathBuf::from(v));
                i += 2;
            }
            "--output" => {
                // Back-compat: accept `--output PATH` as long as PATH
                // isn't an ACLI format keyword (which would have been
                // consumed at the dispatcher level).
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--output needs a path"))?;
                if matches!(v.as_str(), "text" | "json" | "table") {
                    return Err(anyhow!(
                        "`--output {v}` is the ACLI format flag — put it before the subcommand: \
                         `lex --output {v} ast-merge ...`. Use `--write PATH` for the merged source path."));
                }
                o.output = Some(PathBuf::from(v));
                i += 2;
            }
            other => { o.files.push(PathBuf::from(other)); i += 1; }
        }
    }
    Ok(o)
}

fn load_fns(path: &std::path::Path) -> Result<BTreeMap<String, FnDecl>> {
    let prog = load_program(path)
        .map_err(|e| anyhow!("load {}: {e}", path.display()))?;
    let stages = canonicalize_program(&prog);
    let mut out = BTreeMap::new();
    for stage in stages {
        if let Stage::FnDecl(fd) = stage {
            out.insert(fd.name.clone(), fd);
        }
    }
    Ok(out)
}

/// Hash a fn's full structural identity (canonical AST), name normalized.
/// Used to detect "did this fn change?" — if the hash is the same on
/// both sides, the fn is byte-identical AST-wise.
fn fn_hash(fd: &FnDecl) -> String {
    let mut anon = fd.clone();
    anon.name = String::new();
    stage_canonical_hash_hex(&Stage::FnDecl(anon))
}

fn compute_merge(
    base:   &BTreeMap<String, FnDecl>,
    ours:   &BTreeMap<String, FnDecl>,
    theirs: &BTreeMap<String, FnDecl>,
) -> MergeReport {
    let mut report = MergeReport {
        summary: MergeSummary::default(),
        merged: Vec::new(),
        conflicts: Vec::new(),
    };
    let names: std::collections::BTreeSet<&String> = base.keys()
        .chain(ours.keys()).chain(theirs.keys()).collect();

    for name in &names {
        let in_base   = base.contains_key(*name);
        let in_ours   = ours.contains_key(*name);
        let in_theirs = theirs.contains_key(*name);
        match (in_base, in_ours, in_theirs) {
            (true, true, true) => {
                let h_b = fn_hash(&base[*name]);
                let h_o = fn_hash(&ours[*name]);
                let h_t = fn_hash(&theirs[*name]);
                let from = if h_o == h_t {
                    if h_b == h_o { "base" } else { "both" }
                } else if h_b == h_o {
                    "theirs"
                } else if h_b == h_t {
                    "ours"
                } else {
                    report.conflicts.push(Conflict {
                        kind: "modify-modify",
                        name: (*name).clone(),
                        base:   Some(render_fn(&base[*name])),
                        ours:   Some(render_fn(&ours[*name])),
                        theirs: Some(render_fn(&theirs[*name])),
                    });
                    continue;
                };
                report.merged.push(MergedFn { name: (*name).clone(), from });
            }
            (true, true, false) => {
                // Theirs deleted. Conflict only if ours modified.
                if fn_hash(&base[*name]) == fn_hash(&ours[*name]) {
                    // ours unchanged → take theirs' delete (omit).
                } else {
                    report.conflicts.push(Conflict {
                        kind: "modify-delete",
                        name: (*name).clone(),
                        base:   Some(render_fn(&base[*name])),
                        ours:   Some(render_fn(&ours[*name])),
                        theirs: None,
                    });
                }
            }
            (true, false, true) => {
                if fn_hash(&base[*name]) == fn_hash(&theirs[*name]) {
                    // theirs unchanged → take ours' delete.
                } else {
                    report.conflicts.push(Conflict {
                        kind: "delete-modify",
                        name: (*name).clone(),
                        base:   Some(render_fn(&base[*name])),
                        ours:   None,
                        theirs: Some(render_fn(&theirs[*name])),
                    });
                }
            }
            (false, true, true) => {
                // Both added independently. Same body → take either; else add-add conflict.
                if fn_hash(&ours[*name]) == fn_hash(&theirs[*name]) {
                    report.merged.push(MergedFn {
                        name: (*name).clone(), from: "added-both",
                    });
                } else {
                    report.conflicts.push(Conflict {
                        kind: "add-add",
                        name: (*name).clone(),
                        base:   None,
                        ours:   Some(render_fn(&ours[*name])),
                        theirs: Some(render_fn(&theirs[*name])),
                    });
                }
            }
            (false, true, false) => {
                report.merged.push(MergedFn {
                    name: (*name).clone(), from: "added-ours",
                });
            }
            (false, false, true) => {
                report.merged.push(MergedFn {
                    name: (*name).clone(), from: "added-theirs",
                });
            }
            (true, false, false) => {
                // Both branches deleted; clean removal, nothing to merge.
            }
            (false, false, false) => unreachable!(),
        }
    }

    report.summary.clean     = report.merged.len();
    report.summary.conflicts = report.conflicts.len();
    report.summary.total_fns = report.merged.len() + report.conflicts.len();
    report
}

fn pick_fn(
    name: &str,
    base: &BTreeMap<String, FnDecl>,
    ours: &BTreeMap<String, FnDecl>,
    theirs: &BTreeMap<String, FnDecl>,
    from: &str,
) -> FnDecl {
    match from {
        "base"        | "both"          => base[name].clone(),
        "ours"        | "added-ours"    | "added-both" => ours[name].clone(),
        "theirs"      | "added-theirs"  => theirs[name].clone(),
        _ => ours.get(name).or(theirs.get(name)).or(base.get(name)).cloned().unwrap(),
    }
}

fn render_fn(fd: &FnDecl) -> String {
    print_stages(&[Stage::FnDecl(fd.clone())])
}

fn short_kind_message(kind: &str) -> &'static str {
    match kind {
        "modify-modify" => "both sides modified",
        "modify-delete" => "ours modified, theirs deleted",
        "delete-modify" => "ours deleted, theirs modified",
        "add-add"       => "both added with different bodies",
        _ => "",
    }
}
