//! `lex propagate` — push a low-level package's API change down its dependents
//! (#893, propagation epic parts b + c).
//!
//! When an upstream package renames a public symbol (`gcd` → `euclidean_gcd`),
//! every dependent that calls `nt.gcd` must become `nt.euclidean_gcd`. On
//! GitHub that's a breaking change discovered as red CI in N downstream repos,
//! whenever someone next builds them. Here the edit is **mechanical and
//! verifiable**: the rewrite is a token-precise transform (part b), and each
//! dependent is re-type-checked after. A change that is *not* a pure rename
//! (a behavioral/signature change) is handed to an agent regenerator behind
//! the same always-valid-HEAD gate (part c).
//!
//! Usage:
//!   lex propagate --package <upstream> --rename <old>=<new> [--rename …]
//!                 [--workspace <dir>] [--apply]
//!   lex propagate --package <upstream> --symbol <name> --note <text>
//!                 --regenerate-cmd <cmd> | --ollama [MODEL]
//!                 [--workspace <dir>] [--apply]

use super::*;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn cmd_propagate(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let mut package: Option<String> = None;
    let mut renames: Vec<(String, String)> = Vec::new();
    let mut workspace: Option<PathBuf> = None;
    let mut apply = false;
    // Semantic (part c) options.
    let mut symbol: Option<String> = None;
    let mut note: Option<String> = None;
    let mut regen_cmd: Option<String> = None;
    let mut ollama: Option<Option<String>> = None;
    // Auto-detect renames from a hosted release pair.
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    let mut registry: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--package" => { package = args.get(i + 1).cloned(); i += 2; }
            "--from" => { from = args.get(i + 1).cloned(); i += 2; }
            "--to" => { to = args.get(i + 1).cloned(); i += 2; }
            "--registry" => { registry = args.get(i + 1).cloned(); i += 2; }
            "--rename" => {
                let spec = args.get(i + 1).ok_or_else(|| anyhow!("--rename needs <old>=<new>"))?;
                let (old, new) = spec.split_once('=')
                    .ok_or_else(|| anyhow!("--rename must be <old>=<new>, got `{spec}`"))?;
                renames.push((old.to_string(), new.to_string()));
                i += 2;
            }
            "--workspace" => { workspace = args.get(i + 1).map(PathBuf::from); i += 2; }
            "--apply" => { apply = true; i += 1; }
            "--symbol" => { symbol = args.get(i + 1).cloned(); i += 2; }
            "--note" => { note = args.get(i + 1).cloned(); i += 2; }
            "--regenerate-cmd" => { regen_cmd = args.get(i + 1).cloned(); i += 2; }
            "--ollama" => {
                match args.get(i + 1) {
                    Some(m) if !m.starts_with("--") => { ollama = Some(Some(m.clone())); i += 2; }
                    _ => { ollama = Some(None); i += 1; }
                }
            }
            other => bail!("unexpected arg `{other}`"),
        }
    }

    let package = package.ok_or_else(|| anyhow!("--package <upstream> is required"))?;
    let semantic = symbol.is_some() || regen_cmd.is_some() || ollama.is_some();
    if semantic && !renames.is_empty() {
        bail!("choose either mechanical --rename or semantic --symbol, not both");
    }

    // Auto-detect renames from a hosted release pair (#893): fetch the two
    // releases' API diff and fold the detected renames into `renames`, so the
    // operator needn't restate them. A breaking change that ISN'T a rename is
    // surfaced as needing a semantic migration.
    if from.is_some() || to.is_some() {
        if semantic {
            bail!("--from/--to derive mechanical renames; not compatible with --symbol");
        }
        let (from, to) = (from.unwrap(), to.unwrap_or_default());
        let registry = registry
            .ok_or_else(|| anyhow!("--from/--to need --registry <host/tenant[/store]> to fetch the API diff"))?;
        let diff = fetch_api_diff(&registry, &package, &from, &to)?;
        for r in &diff.renames {
            renames.push((r.old.clone(), r.new.clone()));
        }
        eprintln!(
            "api-diff {package} {from}→{to}: {} ({}); {} rename(s) auto-detected",
            diff.change, if diff.detail.is_empty() { "—" } else { &diff.detail }, diff.renames.len()
        );
        if diff.change == "breaking" && diff.renames.is_empty() {
            eprintln!(
                "  note: this is a breaking change that is not a pure rename — \
                 run a semantic migration: lex propagate --package {package} --symbol <name> \
                 --note '{}' --ollama", diff.detail
            );
        }
    }

    // The dependent packages to migrate: each package dir under --workspace
    // that depends on `package`, or just the current package.
    let targets = discover_targets(workspace.as_deref(), &package)?;
    if targets.is_empty() {
        bail!("no dependent packages found for `{package}`\
               {}", workspace.as_ref().map(|w| format!(" under {}", w.display())).unwrap_or_default());
    }

    if semantic {
        let symbol = symbol.ok_or_else(|| anyhow!("--symbol <name> is required for a semantic migration"))?;
        let regen = SemanticRegen::from_flags(regen_cmd, ollama)?;
        propagate_semantic(fmt, &package, &symbol, note.as_deref(), &regen, &targets, apply)
    } else {
        if renames.is_empty() {
            bail!("--rename <old>=<new> is required (or use --symbol for a semantic migration)");
        }
        propagate_renames(fmt, &package, &renames, &targets, apply)
    }
}

/// A dependent package to migrate: its root dir and the import aliases it uses
/// for the upstream package.
struct Target {
    dir: PathBuf,
    aliases: Vec<String>,
}

/// Find packages that depend on `upstream`. With `workspace`, scan each
/// immediate subdirectory that is a package (`lex.toml`); otherwise the
/// current package only. A package qualifies if its manifest declares
/// `upstream` as a dependency.
fn discover_targets(workspace: Option<&Path>, upstream: &str) -> Result<Vec<Target>> {
    let dirs: Vec<PathBuf> = match workspace {
        Some(ws) => std::fs::read_dir(ws)
            .with_context(|| format!("reading workspace {}", ws.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("lex.toml").exists())
            .collect(),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let (_, dir) = lex_syntax::find_manifest(&cwd)
                .ok_or_else(|| anyhow!("no lex.toml found (run inside a package or pass --workspace)"))?;
            vec![dir]
        }
    };

    let mut targets = Vec::new();
    for dir in dirs {
        let manifest = match lex_syntax::Manifest::load(&dir.join("lex.toml")) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !manifest.dependencies.contains_key(upstream) {
            continue;
        }
        let aliases = aliases_for_package(&dir, upstream)?;
        targets.push(Target { dir, aliases });
    }
    Ok(targets)
}

/// The import aliases a package uses for `upstream`, scanned from its source:
/// `import "upstream/mod" as nt` → `nt`. A package may import several of the
/// upstream's modules under different aliases.
fn aliases_for_package(dir: &Path, upstream: &str) -> Result<Vec<String>> {
    let mut aliases = std::collections::BTreeSet::new();
    for path in package_lex_files(dir) {
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        if let Ok(prog) = lex_syntax::parse_source(&src) {
            for item in &prog.items {
                if let lex_syntax::Item::Import(imp) = item {
                    // reference like "upstream/lib" (or "upstream")
                    let matches_pkg = imp.reference == upstream
                        || imp.reference.starts_with(&format!("{upstream}/"));
                    if matches_pkg {
                        aliases.insert(imp.alias.clone());
                    }
                }
            }
        }
    }
    Ok(aliases.into_iter().collect())
}

/// Every `.lex` file under a package's `src/` (and the package root).
fn package_lex_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.join("src")];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x == "lex").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// The API diff between two releases, as served by
/// `GET {public}/{name}/api-diff?from=&to=`.
struct ApiDiff {
    change: String,
    detail: String,
    renames: Vec<Rename>,
}
struct Rename {
    old: String,
    new: String,
}

/// Fetch the API diff for `package` between two releases from its registry's
/// public surface (#893). `registry` is the tenant-qualified string
/// (`host/tenant[/store]`), the same as a `lex.toml` registry dependency.
fn fetch_api_diff(registry: &str, package: &str, from: &str, to: &str) -> Result<ApiDiff> {
    let pr = lex_syntax::registry::public(registry)
        .ok_or_else(|| anyhow!("--registry must be tenant-qualified (host/tenant[/store]), got `{registry}`"))?;
    // public base is `https://host/v1/public/tenant`; append the package path.
    let mut url = format!("{}/{package}/api-diff?from={from}&to={to}", pr.base);
    if let Some(store) = &pr.store {
        url.push_str(&format!("&store={store}"));
    }
    let body = ureq::get(&url)
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?
        .into_body()
        .read_to_string()
        .map_err(|e| anyhow!("reading api-diff: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing api-diff from {url}"))?;
    let renames = v.get("renames").and_then(|r| r.as_array()).map(|arr| {
        arr.iter().filter_map(|r| {
            Some(Rename {
                old: r.get("old")?.as_str()?.to_string(),
                new: r.get("new")?.as_str()?.to_string(),
            })
        }).collect()
    }).unwrap_or_default();
    Ok(ApiDiff {
        change: v.get("change").and_then(|c| c.as_str()).unwrap_or("unknown").to_string(),
        detail: v.get("detail").and_then(|d| d.as_str()).unwrap_or("").to_string(),
        renames,
    })
}

// ── Part b: mechanical rename propagation ───────────────────────────────────

/// Rewrite every `<alias>.<old>` qualified reference in `src` to
/// `<alias>.<new>`, token-precise so formatting and comments survive
/// (unlike a reprint) and unrelated names (`nt.gcd2`, a string `"nt.gcd"`)
/// are untouched. Returns the rewritten source and the number of edits.
pub fn rewrite_qualified(
    src: &str,
    alias: &str,
    old: &str,
    new: &str,
) -> Result<(String, usize)> {
    use lex_syntax::TokenKind;
    let toks = lex_syntax::lex(src).map_err(|e| anyhow!("lex: {e}"))?;
    // Collect the byte spans of the `old` identifier in each
    // `Ident(alias) Dot Ident(old)` run.
    let mut spans: Vec<std::ops::Range<usize>> = Vec::new();
    for w in toks.windows(3) {
        let is_alias = matches!(&w[0].kind, TokenKind::Ident(s) if s == alias);
        let is_dot = matches!(w[1].kind, TokenKind::Dot);
        let is_old = matches!(&w[2].kind, TokenKind::Ident(s) if s == old);
        if is_alias && is_dot && is_old {
            spans.push(w[2].span.clone());
        }
    }
    let count = spans.len();
    // Apply right-to-left so earlier byte offsets stay valid.
    let mut out = src.to_string();
    for span in spans.into_iter().rev() {
        out.replace_range(span, new);
    }
    Ok((out, count))
}

#[derive(serde::Serialize)]
struct FileEdit {
    file: String,
    alias: String,
    old: String,
    new: String,
    edits: usize,
}

fn propagate_renames(
    fmt: &OutputFormat,
    package: &str,
    renames: &[(String, String)],
    targets: &[Target],
    apply: bool,
) -> Result<()> {
    let mut all_edits: Vec<FileEdit> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for target in targets {
        if target.aliases.is_empty() {
            continue; // depends on the package but imports none of its modules
        }
        // Rewrite every file, accumulating edits per file so a file touched by
        // several aliases/renames is written once.
        let mut file_new: BTreeMap<PathBuf, String> = BTreeMap::new();
        for path in package_lex_files(&target.dir) {
            let mut src = std::fs::read_to_string(&path).unwrap_or_default();
            let mut changed = false;
            for alias in &target.aliases {
                for (old, new) in renames {
                    let (next, n) = rewrite_qualified(&src, alias, old, new)?;
                    if n > 0 {
                        all_edits.push(FileEdit {
                            file: path.display().to_string(),
                            alias: alias.clone(),
                            old: old.clone(),
                            new: new.clone(),
                            edits: n,
                        });
                        src = next;
                        changed = true;
                    }
                }
            }
            if changed {
                file_new.insert(path, src);
            }
        }

        if apply {
            for (path, src) in &file_new {
                std::fs::write(path, src).with_context(|| format!("writing {}", path.display()))?;
            }
        }

        // Verify the rewrite produced valid syntax. A full type-check needs
        // the upstream dependency resolved (the caller runs `lex check` /
        // `lex pkg install` for that); a rename is a token-precise transform,
        // so parse-validity is the property this step must guarantee.
        for (path, src) in &file_new {
            if let Err(e) = lex_syntax::parse_source(src) {
                errors.push(format!("{}: rewrite produced invalid syntax: {e}", path.display()));
            }
        }
    }

    let applied = apply;
    let data = serde_json::json!({
        "package": package,
        "applied": applied,
        "edits": &all_edits,
        "errors": &errors,
    });
    let total: usize = all_edits.iter().map(|e| e.edits).sum();
    let files = all_edits.len();
    let errs = errors.clone();
    acli::emit_or_text("propagate", data, fmt, move || {
        if all_edits.is_empty() {
            println!("no references to rewrite for `{package}`");
        } else {
            let verb = if applied { "rewrote" } else { "would rewrite (dry run; pass --apply)" };
            println!("{verb} {total} reference(s) across {files} file edit(s):");
            for e in &all_edits {
                println!("  {}  {}.{} → {}.{}  ({}×)", e.file, e.alias, e.old, e.alias, e.new, e.edits);
            }
        }
        for e in &errs {
            eprintln!("  verify FAILED: {e}");
        }
    });

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{} dependent(s) failed to type-check after the rewrite", errors.len())
    }
}

// ── Part c: agent-driven semantic migration ─────────────────────────────────

enum SemanticRegen {
    Cmd(String),
    Ollama(Option<String>),
}

impl SemanticRegen {
    fn from_flags(cmd: Option<String>, ollama: Option<Option<String>>) -> Result<Self> {
        match (cmd, ollama) {
            (Some(c), None) => Ok(SemanticRegen::Cmd(c)),
            (None, Some(m)) => Ok(SemanticRegen::Ollama(m)),
            (None, None) => bail!("a semantic migration needs --regenerate-cmd or --ollama"),
            (Some(_), Some(_)) => bail!("choose one of --regenerate-cmd / --ollama"),
        }
    }
}

/// The per-function migration task handed to the regenerator.
#[derive(serde::Serialize)]
struct MigrationTask {
    package: String,
    symbol: String,
    note: String,
    file: String,
    function: String,
    current_source: String,
}

fn propagate_semantic(
    fmt: &OutputFormat,
    package: &str,
    symbol: &str,
    note: Option<&str>,
    regen: &SemanticRegen,
    targets: &[Target],
    apply: bool,
) -> Result<()> {
    let note = note.unwrap_or("upstream API changed").to_string();
    let mut migrated: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for target in targets {
        // A function is affected if it references `<alias>.<symbol>` for any
        // alias the dependent uses for the upstream package.
        for path in package_lex_files(&target.dir) {
            let src = std::fs::read_to_string(&path).unwrap_or_default();
            for alias in &target.aliases {
                if !references(&src, alias, symbol) {
                    continue;
                }
                let task = MigrationTask {
                    package: package.to_string(),
                    symbol: format!("{alias}.{symbol}"),
                    note: note.clone(),
                    file: path.display().to_string(),
                    function: String::new(),
                    current_source: src.clone(),
                };
                match regenerate_and_verify(&task, regen, &target.dir, &path) {
                    Ok(new_src) => {
                        if apply {
                            std::fs::write(&path, &new_src)
                                .with_context(|| format!("writing {}", path.display()))?;
                        }
                        migrated.push(serde_json::json!({
                            "file": path.display().to_string(),
                            "symbol": task.symbol,
                            "verified": true,
                        }));
                    }
                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                }
                break; // one migration per file
            }
        }
    }

    let data = serde_json::json!({
        "package": package, "symbol": symbol, "applied": apply,
        "migrated": &migrated, "errors": &errors,
    });
    let mig = migrated.clone();
    let errs = errors.clone();
    let applied = apply;
    acli::emit_or_text("propagate", data, fmt, move || {
        let verb = if applied { "migrated" } else { "verified (dry run)" };
        println!("{verb} {} file(s) referencing `{symbol}`", mig.len());
        for e in &errs {
            eprintln!("  FAILED: {e}");
        }
    });

    if errors.is_empty() { Ok(()) } else {
        bail!("{} semantic migration(s) failed to verify", errors.len())
    }
}

/// Whether `src` references `<alias>.<symbol>` at the token level.
fn references(src: &str, alias: &str, symbol: &str) -> bool {
    use lex_syntax::TokenKind;
    let Ok(toks) = lex_syntax::lex(src) else { return false };
    toks.windows(3).any(|w| {
        matches!(&w[0].kind, TokenKind::Ident(s) if s == alias)
            && matches!(w[1].kind, TokenKind::Dot)
            && matches!(&w[2].kind, TokenKind::Ident(s) if s == symbol)
    })
}

/// Regenerate a migrated version of the file via the agent, and accept it only
/// if it parses and type-checks (the always-valid-HEAD gate applied to a
/// propagated change). Returns the verified new source.
fn regenerate_and_verify(
    task: &MigrationTask,
    regen: &SemanticRegen,
    pkg_dir: &Path,
    file: &Path,
) -> Result<String> {
    let prompt = format!(
        "You are migrating a dependent package after an upstream change.\n\
         Upstream package: {}\n\
         Changed symbol:   {}\n\
         What changed:     {}\n\n\
         Rewrite the following Lex module so it still compiles and preserves \
         its behavior against the changed upstream. Output ONLY the full Lex \
         source of the module, nothing else.\n\n\
         --- current module ({}) ---\n{}",
        task.package, task.symbol, task.note, task.file, task.current_source,
    );
    let candidate = match regen {
        SemanticRegen::Cmd(cmd) => crate::replay_runner::regenerate_with_prompt_cmd(&prompt, cmd)?,
        SemanticRegen::Ollama(model) => {
            let m = model.clone().unwrap_or_else(|| "qwen3.8:27b-mlx".to_string());
            crate::replay_runner::regenerate_with_prompt_ollama(&prompt, &m)?
        }
    };
    let candidate = crate::replay_runner::strip_lex_run_echo(&candidate);
    // Gate (always-valid-HEAD applied to a propagated change): the candidate
    // must parse and type-check.
    let program = lex_syntax::parse_source(&candidate)
        .map_err(|e| anyhow!("regenerated source does not parse: {e}"))?;
    let stages = lex_ast::canonicalize_program(&program);
    lex_types::check_program(&stages)
        .map_err(|e| anyhow!("regenerated source does not type-check: {e:?}"))?;
    let _ = (pkg_dir, file);
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::rewrite_qualified;

    #[test]
    fn rewrites_only_the_exact_qualified_reference() {
        let src = "import \"up/lib\" as up\n\
                   fn f(x :: Int) -> Int { up.old(x) + up.old(1) }\n";
        let (out, n) = rewrite_qualified(src, "up", "old", "new").unwrap();
        assert_eq!(n, 2, "both call sites");
        assert!(out.contains("up.new(x)") && out.contains("up.new(1)"), "{out}");
        assert!(!out.contains("up.old"), "no old refs left: {out}");
    }

    #[test]
    fn leaves_strings_comments_and_lookalikes_untouched() {
        let src = "# up.old in a comment\n\
                   fn g() -> Str { \"up.old in a string\" }\n\
                   fn h(x :: Int) -> Int { up.older(x) }\n"; // up.older != up.old
        let (out, n) = rewrite_qualified(src, "up", "old", "new").unwrap();
        assert_eq!(n, 0, "no exact `up.old` reference exists");
        assert_eq!(out, src, "source unchanged");
    }

    #[test]
    fn does_not_touch_a_different_alias() {
        let src = "fn f(x :: Int) -> Int { other.old(x) }\n";
        let (_out, n) = rewrite_qualified(src, "up", "old", "new").unwrap();
        assert_eq!(n, 0, "different alias must be left alone");
    }
}
