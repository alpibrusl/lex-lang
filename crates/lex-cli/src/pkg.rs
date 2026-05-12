//! `lex pkg` subcommands — package manifest management.
//!
//! Commands:
//!   lex pkg init                    — create a starter lex.toml in the CWD
//!   lex pkg add <name> --path <p>   — add a path dependency
//!   lex pkg add <name> --git  <url> — add a git dependency (clones on first use)
//!   lex pkg list                    — list dependencies in lex.toml

use anyhow::{bail, Result};
use std::path::Path;

pub fn cmd_pkg(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("init")    => cmd_init(),
        Some("add")     => cmd_add(&args[1..]),
        Some("list")    => cmd_list(),
        Some(other)     => bail!("unknown pkg subcommand `{other}`; try: init, add, list"),
        None            => bail!("usage: lex pkg <init|add|list>"),
    }
}

// ── lex pkg init ──────────────────────────────────────────────────────────────

fn cmd_init() -> Result<()> {
    let toml_path = Path::new("lex.toml");
    if toml_path.exists() {
        bail!("lex.toml already exists");
    }
    let cwd_name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "my-project".to_string());
    let content = format!(
        r#"[package]
name = "{cwd_name}"
version = "0.1.0"

[dependencies]
# lex-schema = {{ path = "../lex-schema" }}
# lex-schema = {{ git = "https://github.com/alpibrusl/lex-schema" }}
"#
    );
    std::fs::write(toml_path, content)?;
    println!("created lex.toml");
    Ok(())
}

// ── lex pkg add ───────────────────────────────────────────────────────────────

fn cmd_add(args: &[String]) -> Result<()> {
    // Usage: lex pkg add <name> (--path <p> | --git <url>)
    let name = args.first().ok_or_else(|| anyhow::anyhow!(
        "usage: lex pkg add <name> (--path <p> | --git <url>)"))?;

    let mut path_val: Option<String> = None;
    let mut git_val:  Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                i += 1;
                path_val = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--path requires a value")
                })?);
            }
            "--git" => {
                i += 1;
                git_val = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--git requires a value")
                })?);
            }
            other => bail!("unknown flag `{other}`"),
        }
        i += 1;
    }

    let dep_entry = match (path_val, git_val) {
        (Some(p), None) => format!(r#"{{ path = "{p}" }}"#),
        (None, Some(u)) => format!(r#"{{ git = "{u}" }}"#),
        (Some(_), Some(_)) => bail!("specify either --path or --git, not both"),
        (None, None) => bail!("usage: lex pkg add <name> (--path <p> | --git <url>)"),
    };

    upsert_dependency(name, &dep_entry)
}

// ── lex pkg list ──────────────────────────────────────────────────────────────

fn cmd_list() -> Result<()> {
    let (toml_path, _) = lex_syntax::find_manifest(
        &std::env::current_dir().unwrap_or_else(|_| ".".into())
    ).ok_or_else(|| anyhow::anyhow!("no lex.toml found"))?;

    let manifest = lex_syntax::Manifest::load(&toml_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if manifest.dependencies.is_empty() {
        println!("no dependencies in {}", toml_path.display());
        return Ok(());
    }
    println!("dependencies (from {}):", toml_path.display());
    for (name, dep) in &manifest.dependencies {
        match dep {
            lex_syntax::workspace::Dependency::Path { path } =>
                println!("  {name}  path = {path}"),
            lex_syntax::workspace::Dependency::Git { git } =>
                println!("  {name}  git  = {git}"),
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Insert or replace a `[dependencies]` entry in lex.toml.
///
/// Naive line-based edit: preserves comments and formatting.  If the
/// key already exists its line is replaced; otherwise the entry is
/// appended inside the `[dependencies]` section (creating the section
/// if absent).
fn upsert_dependency(name: &str, entry: &str) -> Result<()> {
    let toml_path = locate_or_create_toml()?;
    let raw = std::fs::read_to_string(&toml_path)?;
    let new_line = format!("{name} = {entry}");
    let key_prefix = format!("{name} =");

    // Try to replace an existing line.
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    if let Some(idx) = lines.iter().position(|l| l.trim_start().starts_with(&key_prefix)) {
        lines[idx] = new_line.clone();
        std::fs::write(&toml_path, lines.join("\n") + "\n")?;
        println!("updated {name} in {}", toml_path.display());
        return Ok(());
    }

    // Append inside [dependencies] section, or add the section first.
    if let Some(idx) = lines.iter().position(|l| l.trim() == "[dependencies]") {
        // Insert after the [dependencies] header.
        lines.insert(idx + 1, new_line.clone());
    } else {
        lines.push(String::new());
        lines.push("[dependencies]".to_string());
        lines.push(new_line.clone());
    }
    std::fs::write(&toml_path, lines.join("\n") + "\n")?;
    println!("added {name} to {}", toml_path.display());
    Ok(())
}

fn locate_or_create_toml() -> Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    if let Some((path, _)) = lex_syntax::find_manifest(&cwd) {
        return Ok(path);
    }
    // No lex.toml found — offer to create one in cwd.
    let path = cwd.join("lex.toml");
    let name = cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    std::fs::write(&path, format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[dependencies]\n"
    ))?;
    println!("created {}", path.display());
    Ok(path)
}
