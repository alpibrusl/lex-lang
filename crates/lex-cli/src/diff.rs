//! `lex diff` — AST-native structural diff between two Lex sources.
//!
//! Pitch: Git diffs are line-based. Renaming a function or moving a
//! match arm produces 50 "deleted" lines and 50 "inserted" lines that
//! a reviewing agent has to re-parse. `lex diff` walks the canonical
//! AST and reports *what changed in tree shape*: stages added,
//! removed, modified — and inside a modified body, which expression
//! kinds were replaced.
//!
//! Output is plain text by default and JSON via `--json` for piping
//! into another agent's eval loop. Default JSON shape:
//!
//! ```json
//! {
//!   "added":   [{"name": "...", "signature": "..."}],
//!   "removed": [{"name": "...", "signature": "..."}],
//!   "renamed": [{"from": "...", "to": "...", "signature": "..."}],
//!   "modified": [
//!     {
//!       "name": "...",
//!       "signature_before": "...",
//!       "signature_after":  "...",
//!       "signature_changed": true|false,
//!       "effect_changes": {
//!         "before":  ["..."],
//!         "after":   ["..."],
//!         "added":   ["..."],
//!         "removed": ["..."]
//!       },
//!       "body_patches": [{"op": "Replace", "node_path": "...",
//!                         "from_kind": "...", "to_kind": "..."}]
//!     }
//!   ]
//! }
//! ```
//!
//! Effect changes are surfaced as a dedicated field — separate from
//! `signature_changed` — because for security review they're the
//! single most important kind of change. A reviewer (human or agent)
//! scanning a diff for "did this fn newly gain `[net]`?" should not
//! have to re-parse the rendered signature string.

use crate::acli as acli_mod;
use ::acli::OutputFormat;
use anyhow::{anyhow, Context, Result};
use lex_ast::{canonicalize_program, FnDecl, Stage};
use lex_syntax::parse_source;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

// Re-export from lex-vcs so callers that previously imported from
// this module (e.g. `use crate::diff::compute_diff`) continue to work.
pub use lex_vcs::compute_diff;

#[derive(Default)]
struct DiffOpts {
    files: Vec<PathBuf>,
    json: bool,
    /// Also emit body-level patches for modified fns. On by default
    /// because that's the wow; off via --no-body if the caller wants
    /// just signature-level deltas.
    body_patches: bool,
}

pub fn cmd_diff(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let opts = parse_diff_args(args)?;
    if opts.files.len() != 2 {
        return Err(anyhow!("usage: lex diff <file_a> <file_b> [--json] [--no-body]"));
    }
    let a = load_fns(&opts.files[0])?;
    let b = load_fns(&opts.files[1])?;
    let report = compute_diff(&a, &b, opts.body_patches);

    if matches!(fmt, OutputFormat::Json) {
        let data = serde_json::to_value(&report)?;
        acli_mod::emit_or_text("ast-diff", data, fmt, || {});
        return Ok(());
    }
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if report.added.is_empty() && report.removed.is_empty()
        && report.renamed.is_empty() && report.modified.is_empty()
    {
        println!("(no structural changes)");
        return Ok(());
    }

    for a in &report.added   { println!("+ added    {}", a.signature); }
    for r in &report.removed { println!("- removed  {}", r.signature); }
    for r in &report.renamed {
        println!("→ renamed  {} → {}", r.from, r.to);
        println!("           {}", r.signature);
    }
    for m in &report.modified {
        let sig = if m.signature_changed {
            format!("{}\n             {} ", m.signature_before, "→")
                + &m.signature_after
        } else {
            m.signature_after.clone()
        };
        println!("~ modified {sig}");
        if !m.effect_changes.added.is_empty() {
            println!("             ⚠ effects gained: [{}]",
                m.effect_changes.added.join(", "));
        }
        if !m.effect_changes.removed.is_empty() {
            println!("             ✓ effects dropped: [{}]",
                m.effect_changes.removed.join(", "));
        }
        for p in &m.body_patches {
            if p.from_kind == p.to_kind {
                println!("             @ {}: {} edited", p.node_path, p.from_kind);
            } else {
                println!("             @ {}: {} → {}",
                    p.node_path, p.from_kind, p.to_kind);
            }
        }
    }
    Ok(())
}

fn parse_diff_args(args: &[String]) -> Result<DiffOpts> {
    let mut o = DiffOpts { body_patches: true, ..Default::default() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json"    => { o.json = true;          i += 1; }
            "--no-body" => { o.body_patches = false; i += 1; }
            other => { o.files.push(PathBuf::from(other)); i += 1; }
        }
    }
    Ok(o)
}

fn load_fns(path: &std::path::Path) -> Result<BTreeMap<String, FnDecl>> {
    let src = fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let prog = parse_source(&src)
        .map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
    let stages = canonicalize_program(&prog);
    let mut out = BTreeMap::new();
    for stage in stages {
        if let Stage::FnDecl(fd) = stage {
            out.insert(fd.name.clone(), fd);
        }
    }
    Ok(out)
}

// compute_diff and its helpers have moved to lex-vcs::compute_diff.
// Re-exported above as `pub use lex_vcs::compute_diff`.
