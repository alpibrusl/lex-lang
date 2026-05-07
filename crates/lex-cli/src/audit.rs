//! `lex audit` — structural code search across one or more Lex files.
//!
//! Pitch: when an LLM writes the code, the function signature is the
//! contract. `lex audit` queries that contract:
//!
//!     lex audit examples/                      # summary by effect set
//!     lex audit --effect net,fs_read examples/ # everything touching net or fs_read
//!     lex audit --calls net.post examples/     # every fn that calls net.post
//!     lex audit --uses-host attacker.com .     # any string literal containing this host
//!     lex audit --kind Match examples/         # AST-kind filter
//!
//! Output is plain text by default and JSON with `--json`. The
//! defaults are tuned for piping into another agent's eval loop:
//! one fn per line, fully-qualified.

use crate::acli as acli_mod;
use ::acli::OutputFormat;
use anyhow::{anyhow, Context, Result};
use lex_ast::{canonicalize_program, CExpr, CLit, Effect, FnDecl, Stage, TypeExpr};
use lex_syntax::load_program;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct AuditOpts {
    paths: Vec<PathBuf>,
    /// Effect kinds to filter by. Set semantics: include the fn if *any*
    /// of its effects appear here. Empty = no filter.
    effects: Vec<String>,
    /// Fully-qualified callee names like "net.post" or "io.read".
    /// Match is on the literal trailing `.fn` of any FieldAccess
    /// chain inside the body.
    calls: Vec<String>,
    /// Hostnames (or any substring) to look for inside string literals.
    /// Surfaces `--allow-net-host`-relevant references at audit time.
    hosts: Vec<String>,
    /// AST node-kind name (e.g. "Match", "Lambda", "Constructor").
    kinds: Vec<String>,
    json: bool,
    no_summary: bool,
    /// Store root for `EffectAudit` attestation persistence (#132).
    /// When set together with `--effect K`, every scanned FnDecl gets
    /// an attestation against its content-hashed StageId:
    ///
    /// * `Passed` if it doesn't touch any of the listed effects
    /// * `Failed` if it does (detail = which effect(s))
    ///
    /// Without `--store`, behavior is unchanged.
    store_root: Option<PathBuf>,
    /// Semantic-search query (#224). When set, the audit runs against
    /// the store rather than the file system: every active stage is
    /// embedded into the three-index, then ranked by fused cosine
    /// similarity to this query.
    query: Option<String>,
    /// Top-K cap for `--query` results.
    limit: usize,
}

#[derive(Serialize)]
struct FnHit {
    file: String,
    name: String,
    effects: Vec<String>,
    signature: String,
    /// Why this fn matched the filter (one or more reasons).
    matched: Vec<String>,
}

#[derive(Serialize)]
struct AuditReport {
    summary: BTreeMap<String, usize>,
    hits: Vec<FnHit>,
}

pub fn cmd_audit(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut opts = parse_audit_args(args)?;
    if matches!(fmt, OutputFormat::Json) { opts.json = true; }
    if opts.query.is_some() {
        return cmd_audit_semantic(fmt, &opts);
    }
    if opts.paths.is_empty() {
        return Err(anyhow!("usage: lex audit [paths...] [--effect KIND] [--calls FN] [--uses-host HOST] [--kind NODE] [--query Q] [--json]"));
    }
    let files = collect_lex_files(&opts.paths)?;
    let mut report = AuditReport { summary: BTreeMap::new(), hits: Vec::new() };

    // #132: when `--store DIR` is combined with `--effect K1,K2,...`,
    // every scanned FnDecl gets an EffectAudit attestation against
    // its content-hashed StageId. Without `--effect`, the attestation
    // would be a vacuous claim — refuse so callers don't accumulate
    // noise.
    let att_log = match (&opts.store_root, opts.effects.is_empty()) {
        (Some(_), true) => {
            return Err(anyhow!(
                "`--store DIR` requires `--effect K1,K2,...` to specify the effect set the audit is checking against"));
        }
        (Some(root), false) => {
            let store = lex_store::Store::open(root)
                .with_context(|| format!("opening store at {}", root.display()))?;
            Some(store.attestation_log()?)
        }
        (None, _) => None,
    };

    for path in &files {
        // Parse + canonicalize. Errors are printed once, then we keep
        // going — partial audits over a half-broken codebase are
        // strictly more useful than refusing to run.
        let prog = match load_program(path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("warning: load error in {}: {e}", path.display());
                continue;
            }
        };
        let stages = canonicalize_program(&prog);
        for stage in &stages {
            if let Stage::FnDecl(fd) = stage {
                let info = scan_fn(fd);
                let key = effect_key(&info.effects);
                *report.summary.entry(key).or_insert(0) += 1;

                let matched = filter_match(&opts, fd, &info);
                let always = opts.effects.is_empty()
                    && opts.calls.is_empty()
                    && opts.hosts.is_empty()
                    && opts.kinds.is_empty();
                if !matched.is_empty() || always {
                    report.hits.push(FnHit {
                        file: path.display().to_string(),
                        name: fd.name.clone(),
                        effects: info.effects.clone(),
                        signature: render_signature(fd),
                        matched: if always { vec!["all".into()] } else { matched },
                    });
                }

                if let Some(log) = &att_log {
                    if let Some(stage_id) = lex_ast::stage_id(stage) {
                        let touched: Vec<&String> = info.effects.iter()
                            .filter(|e| opts.effects.iter().any(|q| q == *e))
                            .collect();
                        let result = if touched.is_empty() {
                            lex_vcs::AttestationResult::Passed
                        } else {
                            let names: Vec<&str> = touched.iter().map(|s| s.as_str()).collect();
                            lex_vcs::AttestationResult::Failed {
                                detail: format!("touches forbidden effect(s): {}", names.join(",")),
                            }
                        };
                        let producer = lex_vcs::ProducerDescriptor {
                            tool: "lex audit".into(),
                            version: env!("CARGO_PKG_VERSION").into(),
                            model: None,
                        };
                        let attestation = lex_vcs::Attestation::new(
                            stage_id,
                            None,
                            None,
                            lex_vcs::AttestationKind::EffectAudit,
                            result,
                            producer,
                            None,
                        );
                        if let Err(e) = log.put(&attestation) {
                            eprintln!(
                                "warning: failed to persist EffectAudit attestation in {}: {e}",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
    }

    if matches!(fmt, OutputFormat::Json) {
        // Top-level `--output json` → ACLI envelope.
        let data = serde_json::to_value(&report)?;
        acli_mod::emit_or_text("audit", data, fmt, || {});
        return Ok(());
    }
    if opts.json {
        // Legacy `--json` (without `--output json`): raw report.
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if !opts.no_summary {
        println!("SUMMARY: {} stages across {} files",
            report.summary.values().sum::<usize>(), files.len());
        let max_key = report.summary.keys().map(|k| k.len()).max().unwrap_or(0);
        for (k, v) in &report.summary {
            let pad = ".".repeat((max_key + 4).saturating_sub(k.len()));
            println!("  {k} {pad} {v} stages");
        }
        println!();
    }

    for hit in &report.hits {
        let why = if opts.effects.is_empty() && opts.calls.is_empty()
            && opts.hosts.is_empty() && opts.kinds.is_empty() {
            String::new()
        } else {
            format!("  [{}]", hit.matched.join(", "))
        };
        println!("{}::{}{why}", hit.file, hit.name);
        println!("  {}", hit.signature);
    }
    Ok(())
}

// ---- arg parsing ---------------------------------------------------

fn parse_audit_args(args: &[String]) -> Result<AuditOpts> {
    let mut o = AuditOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--effect" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--effect needs a value"))?;
                o.effects.extend(v.split(',').map(String::from));
                i += 2;
            }
            "--calls" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--calls needs a value"))?;
                o.calls.extend(v.split(',').map(String::from));
                i += 2;
            }
            "--uses-host" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--uses-host needs a value"))?;
                o.hosts.extend(v.split(',').map(String::from));
                i += 2;
            }
            "--kind" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--kind needs a value"))?;
                o.kinds.extend(v.split(',').map(String::from));
                i += 2;
            }
            "--json"        => { o.json = true;        i += 1; }
            "--no-summary"  => { o.no_summary = true;  i += 1; }
            "--store" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--store needs a path"))?;
                o.store_root = Some(PathBuf::from(v));
                i += 2;
            }
            "--query" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--query needs a value"))?;
                o.query = Some(v.clone());
                i += 2;
            }
            "--limit" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--limit needs a value"))?;
                o.limit = v.parse().with_context(|| format!("--limit must be a positive integer (got `{v}`)"))?;
                i += 2;
            }
            other => { o.paths.push(PathBuf::from(other)); i += 1; }
        }
    }
    Ok(o)
}

// ---- file discovery ------------------------------------------------

fn collect_lex_files(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for p in paths {
        let meta = fs::metadata(p)
            .with_context(|| format!("stat {}", p.display()))?;
        if meta.is_file() {
            if p.extension().is_some_and(|e| e == "lex") {
                out.push(p.clone());
            }
        } else if meta.is_dir() {
            walk_dir(p, &mut out);
        }
    }
    out.sort();
    Ok(out)
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip target/ and hidden dirs by default.
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" { continue; }
            walk_dir(&path, out);
        } else if path.extension().is_some_and(|e| e == "lex") {
            out.push(path);
        }
    }
}

// ---- analysis ------------------------------------------------------

#[derive(Default)]
struct FnInfo {
    effects: Vec<String>,
    calls: Vec<String>,
    string_lits: Vec<String>,
    kinds: Vec<&'static str>,
}

fn scan_fn(fd: &FnDecl) -> FnInfo {
    let mut info = FnInfo {
        effects: fd.effects.iter().map(|e: &Effect| e.name.clone()).collect(),
        ..Default::default()
    };
    walk_expr(&fd.body, &mut info);
    info.calls.sort();
    info.calls.dedup();
    info.effects.sort();
    info.effects.dedup();
    info
}

fn walk_expr(e: &CExpr, out: &mut FnInfo) {
    out.kinds.push(node_kind(e));
    match e {
        CExpr::Literal { value: CLit::Str { value } } => {
            out.string_lits.push(value.clone());
        }
        CExpr::Literal { .. } | CExpr::Var { .. } => {}
        CExpr::Call { callee, args } => {
            if let Some(qn) = qualified_callee(callee) {
                out.calls.push(qn);
            }
            walk_expr(callee, out);
            for a in args { walk_expr(a, out); }
        }
        CExpr::Let { value, body, .. } => { walk_expr(value, out); walk_expr(body, out); }
        CExpr::Match { scrutinee, arms } => {
            walk_expr(scrutinee, out);
            for arm in arms { walk_expr(&arm.body, out); }
        }
        CExpr::Block { statements, result } => {
            for s in statements { walk_expr(s, out); }
            walk_expr(result, out);
        }
        CExpr::Constructor { args, .. } => for a in args { walk_expr(a, out); },
        CExpr::RecordLit { fields } => for f in fields { walk_expr(&f.value, out); },
        CExpr::TupleLit { items } | CExpr::ListLit { items } => {
            for it in items { walk_expr(it, out); }
        }
        CExpr::FieldAccess { value, .. } => walk_expr(value, out),
        CExpr::Lambda { body, effects, .. } => {
            // Lambda effects propagate to the enclosing fn (the type
            // checker enforces this); lift them so audit accurately
            // reflects what the function can do.
            for e in effects { out.effects.push(e.name.clone()); }
            walk_expr(body, out);
        }
        CExpr::BinOp { lhs, rhs, .. } => { walk_expr(lhs, out); walk_expr(rhs, out); }
        CExpr::UnaryOp { expr, .. } => walk_expr(expr, out),
        CExpr::Return { value } => walk_expr(value, out),
    }
}

/// If a callee expression is a `module.fn` field access, return
/// "module.fn"; otherwise None.
fn qualified_callee(callee: &CExpr) -> Option<String> {
    if let CExpr::FieldAccess { value, field } = callee {
        if let CExpr::Var { name } = value.as_ref() {
            return Some(format!("{name}.{field}"));
        }
    }
    None
}

fn node_kind(e: &CExpr) -> &'static str {
    match e {
        CExpr::Literal { .. }    => "Literal",
        CExpr::Var { .. }        => "Var",
        CExpr::Call { .. }       => "Call",
        CExpr::Let { .. }        => "Let",
        CExpr::Match { .. }      => "Match",
        CExpr::Block { .. }      => "Block",
        CExpr::Constructor { .. } => "Constructor",
        CExpr::RecordLit { .. }  => "RecordLit",
        CExpr::TupleLit { .. }   => "TupleLit",
        CExpr::ListLit { .. }    => "ListLit",
        CExpr::FieldAccess { .. } => "FieldAccess",
        CExpr::Lambda { .. }     => "Lambda",
        CExpr::BinOp { .. }      => "BinOp",
        CExpr::UnaryOp { .. }    => "UnaryOp",
        CExpr::Return { .. }     => "Return",
    }
}

// ---- filtering -----------------------------------------------------

fn filter_match(opts: &AuditOpts, fd: &FnDecl, info: &FnInfo) -> Vec<String> {
    let mut reasons = Vec::new();
    if !opts.effects.is_empty() {
        let hits: Vec<&String> = info.effects.iter()
            .filter(|e| opts.effects.iter().any(|q| q == *e)).collect();
        if !hits.is_empty() {
            reasons.push(format!("effect={}", hits.iter().map(|s| s.as_str())
                .collect::<Vec<_>>().join(",")));
        }
    }
    if !opts.calls.is_empty() {
        let hits: Vec<&String> = info.calls.iter()
            .filter(|c| opts.calls.iter().any(|q| q == *c)).collect();
        if !hits.is_empty() {
            reasons.push(format!("calls={}", hits.iter().map(|s| s.as_str())
                .collect::<Vec<_>>().join(",")));
        }
    }
    if !opts.hosts.is_empty() {
        let hits: Vec<&String> = info.string_lits.iter()
            .filter(|s| opts.hosts.iter().any(|h| s.contains(h))).collect();
        if !hits.is_empty() {
            reasons.push(format!("uses-host=\"{}\"", hits[0]));
        }
    }
    if !opts.kinds.is_empty() {
        for k in &opts.kinds {
            if info.kinds.contains(&k.as_str()) {
                reasons.push(format!("kind={k}"));
                break;
            }
        }
    }
    let _ = fd;
    reasons
}

// ---- pretty rendering ----------------------------------------------

fn effect_key(effects: &[String]) -> String {
    if effects.is_empty() { "pure".into() }
    else {
        let mut sorted = effects.to_vec();
        sorted.sort();
        sorted.dedup();
        format!("[{}]", sorted.join(", "))
    }
}

fn render_signature(fd: &FnDecl) -> String {
    let params: Vec<String> = fd.params.iter()
        .map(|p| format!("{} :: {}", p.name, render_type(&p.ty))).collect();
    let eff = if fd.effects.is_empty() { String::new() } else {
        let names: Vec<&str> = fd.effects.iter().map(|e| e.name.as_str()).collect();
        format!("[{}] ", names.join(", "))
    };
    format!("fn {}({}) -> {}{}", fd.name, params.join(", "),
        eff, render_type(&fd.return_type))
}

fn render_type(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named { name, args } => {
            if args.is_empty() { name.clone() }
            else {
                let parts: Vec<String> = args.iter().map(render_type).collect();
                format!("{name}[{}]", parts.join(", "))
            }
        }
        TypeExpr::Tuple { items } => {
            let parts: Vec<String> = items.iter().map(render_type).collect();
            format!("({})", parts.join(", "))
        }
        TypeExpr::Record { fields } => {
            let parts: Vec<String> = fields.iter()
                .map(|f| format!("{} :: {}", f.name, render_type(&f.ty))).collect();
            format!("{{ {} }}", parts.join(", "))
        }
        TypeExpr::Function { params, effects, ret } => {
            let parts: Vec<String> = params.iter().map(render_type).collect();
            let eff = if effects.is_empty() { String::new() } else {
                let names: Vec<&str> = effects.iter().map(|e| e.name.as_str()).collect();
                format!("[{}] ", names.join(", "))
            };
            format!("({}) -> {}{}", parts.join(", "), eff, render_type(ret))
        }
        TypeExpr::Union { variants } => {
            let parts: Vec<String> = variants.iter().map(|v| {
                match &v.payload {
                    Some(p) => format!("{}({})", v.name, render_type(p)),
                    None => v.name.clone(),
                }
            }).collect();
            parts.join(" | ")
        }
        TypeExpr::Refined { base, binding, .. } => {
            // Compact diagnostic form (#209 slice 1). Full predicate
            // is in the canonical AST and contributes to OpId hashing.
            format!("{}{{{} | …}}", render_type(base), binding)
        }
    }
}

// ---- #224 semantic search ----------------------------------------

/// `lex audit --query "..."` mode. Walks the store, embeds every
/// active stage, ranks against the query, and applies the structural
/// filters (`--effect`, etc.) as a post-filter on the ranked list.
fn cmd_audit_semantic(fmt: &OutputFormat, opts: &AuditOpts) -> Result<()> {
    let query = opts.query.as_deref().unwrap();
    let limit = if opts.limit == 0 { 10 } else { opts.limit };
    let root = opts.store_root.clone()
        .unwrap_or_else(default_store_root);

    let store = lex_store::Store::open(&root)
        .with_context(|| format!("opening store at {}", root.display()))?;
    let embedder = lex_search::MockEmbedder::new();
    let idx = lex_search::SearchIndex::build(&store, &embedder)
        .map_err(|e| anyhow!("building search index: {e}"))?;
    let mut hits = idx.query(&embedder, query, limit.saturating_mul(4))
        .map_err(|e| anyhow!("query embedding: {e}"))?;

    // Post-filter by --effect on the ranked list. We over-fetch
    // (4× limit) above so that a filter that matches a few stages
    // still has enough candidates after pruning.
    if !opts.effects.is_empty() {
        hits.retain(|h| {
            let stg = match store.get_ast(&h.stage_id) {
                Ok(s) => s,
                Err(_) => return false,
            };
            if let Stage::FnDecl(fd) = stg {
                fd.effects.iter().any(|e| opts.effects.iter().any(|q| q == &e.name))
            } else {
                false
            }
        });
    }
    hits.truncate(limit);

    let v = serde_json::json!({
        "query": query,
        "limit": limit,
        "indexed": idx.stages.len(),
        "hits": serde_json::to_value(&hits)?,
    });
    if matches!(fmt, OutputFormat::Json) {
        crate::acli::emit_or_text("audit", v, fmt, || {});
        return Ok(());
    }
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("{} hit(s) for `{query}`", hits.len());
    for h in &hits {
        println!(
            "  {:>6.3}  {}::{}  {}",
            h.score.fused, h.stage_id, h.name, h.signature
        );
        if let Some(d) = &h.description { println!("          note: {d}"); }
    }
    Ok(())
}

fn default_store_root() -> PathBuf {
    std::env::var("LEX_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".lex"))
}
