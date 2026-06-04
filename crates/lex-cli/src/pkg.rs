//! `lex pkg` subcommands — package manifest management.
//!
//! Commands:
//!   lex pkg init                                       — create a starter lex.toml in the CWD
//!   lex pkg add <name> --path <p>                      — add a path dependency
//!   lex pkg add <name> --git <url> [--tag|--branch|--rev <ref>]
//!                                                      — add a pinned git dependency
//!   lex pkg add <name> --registry <url> --version <v>  — add a registry dependency
//!   lex pkg list                                       — list dependencies in lex.toml
//!   lex pkg publish [--registry <url>] [--token <jwt>] — publish package to a registry

use anyhow::{bail, Result};
use std::path::Path;

pub fn cmd_pkg(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("init")    => cmd_init(),
        Some("add")     => cmd_add(&args[1..]),
        Some("list")    => cmd_list(),
        Some("install") => cmd_install(),
        Some("publish") => cmd_publish(&args[1..]),
        Some(other)     => bail!("unknown pkg subcommand `{other}`; try: init, add, install, list, publish"),
        None            => bail!("usage: lex pkg <init|add|install|list|publish>"),
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
# lex-schema = {{ git = "https://github.com/alpibrusl/lex-schema", tag = "v1.0.0" }}
"#
    );
    std::fs::write(toml_path, content)?;
    println!("created lex.toml");
    Ok(())
}

// ── lex pkg add ───────────────────────────────────────────────────────────────

fn cmd_add(args: &[String]) -> Result<()> {
    // Usage: lex pkg add <name> (--path <p> | --git <url> [--tag|--branch|--rev <ref>] | --registry <url> --version <v>)
    let name = args.first().ok_or_else(|| anyhow::anyhow!(
        "usage: lex pkg add <name> (--path <p> | --git <url> [--tag|--branch|--rev <ref>] | --registry <url> --version <v>)"))?;

    let mut path_val:     Option<String> = None;
    let mut git_val:      Option<String> = None;
    let mut branch_val:   Option<String> = None;
    let mut tag_val:      Option<String> = None;
    let mut rev_val:      Option<String> = None;
    let mut registry_val: Option<String> = None;
    let mut version_val:  Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                i += 1;
                path_val = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("--path requires a value"))?);
            }
            "--git" => {
                i += 1;
                git_val = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("--git requires a value"))?);
            }
            "--branch" => {
                i += 1;
                branch_val = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("--branch requires a value"))?);
            }
            "--tag" => {
                i += 1;
                tag_val = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("--tag requires a value"))?);
            }
            "--rev" => {
                i += 1;
                rev_val = Some(args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("--rev requires a value"))?);
            }
            "--registry" => {
                i += 1;
                registry_val = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--registry requires a value")
                })?);
            }
            "--version" => {
                i += 1;
                version_val = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--version requires a value")
                })?);
            }
            other => bail!("unknown flag `{other}`"),
        }
        i += 1;
    }

    // Validate: --branch/--tag/--rev only make sense with --git, and are mutually exclusive.
    let has_ref = branch_val.is_some() || tag_val.is_some() || rev_val.is_some();
    if has_ref && git_val.is_none() {
        bail!("--branch, --tag, and --rev require --git");
    }
    let ref_count = [&branch_val, &tag_val, &rev_val].iter().filter(|o| o.is_some()).count();
    if ref_count > 1 {
        bail!("at most one of --branch, --tag, --rev may be specified");
    }

    let dep_entry = match (path_val, git_val, registry_val, version_val) {
        (Some(p), None, None, None) => format!(r#"{{ path = "{p}" }}"#),
        (None, Some(u), None, None) => {
            let mut parts = format!(r#"{{ git = "{u}""#);
            if let Some(b) = branch_val { parts.push_str(&format!(r#", branch = "{b}""#)); }
            if let Some(t) = tag_val    { parts.push_str(&format!(r#", tag = "{t}""#)); }
            if let Some(r) = rev_val    { parts.push_str(&format!(r#", rev = "{r}""#)); }
            parts.push_str(" }");
            parts
        }
        (None, None, Some(r), Some(v)) => format!(r#"{{ registry = "{r}", version = "{v}" }}"#),
        (None, None, Some(_), None) => bail!("--registry requires --version"),
        (None, None, None, Some(_)) => bail!("--version requires --registry"),
        _ => bail!("specify exactly one of --path, --git, or --registry (with --version)"),
    };

    upsert_dependency(name, &dep_entry)
}

// ── lex pkg install ───────────────────────────────────────────────────────────

/// Resolve and install all dependencies declared in the nearest lex.toml.
///
/// For path dependencies: verify the directory exists.
/// For git dependencies: clone into the cache directory if not already present.
fn cmd_install() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let (toml_path, toml_dir) = lex_syntax::find_manifest(&cwd)
        .ok_or_else(|| anyhow::anyhow!("no lex.toml found (run `lex pkg init` to create one)"))?;

    let manifest = lex_syntax::Manifest::load(&toml_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if manifest.dependencies.is_empty() {
        println!("no dependencies declared in {}", toml_path.display());
        return Ok(());
    }

    println!("installing dependencies from {}:", toml_path.display());
    let mut errors = Vec::new();

    for (name, dep) in &manifest.dependencies {
        match dep {
            lex_syntax::workspace::Dependency::Path { path } => {
                let full = toml_dir.join(path);
                if full.exists() {
                    println!("  {} (path)  ok — {}", name, full.display());
                } else {
                    let msg = format!("  {} (path)  NOT FOUND: {}", name, full.display());
                    eprintln!("{msg}");
                    errors.push(msg);
                }
            }
            lex_syntax::workspace::Dependency::Git { git, branch, tag, rev } => {
                let ref_desc = branch.as_deref()
                    .map(|b| format!(" branch={b}"))
                    .or_else(|| tag.as_deref().map(|t| format!(" tag={t}")))
                    .or_else(|| rev.as_deref().map(|r| format!(" rev={}", &r[..r.len().min(12)])))
                    .unwrap_or_default();
                print!("  {} (git)      {}{} ... ", name, git, ref_desc);
                // resolve_package_import triggers git_ensure_cached internally.
                // We use a dummy module name — we only care about the side-effect.
                let dummy_file = toml_dir.join("__install_probe__.lex");
                match lex_syntax::workspace::resolve_package_import(&dummy_file, name, "__probe__") {
                    Ok(_) | Err(lex_syntax::PackageError::ModuleNotFound { .. }) => {
                        println!("ok");
                    }
                    Err(e) => {
                        println!("FAILED");
                        eprintln!("    {e}");
                        errors.push(format!("{name}: {e}"));
                    }
                }
            }
            lex_syntax::workspace::Dependency::Registry { registry, version } => {
                print!("  {} (registry) {}@{} ... ", name, registry, version);
                let dummy_file = toml_dir.join("__install_probe__.lex");
                match lex_syntax::workspace::resolve_package_import(&dummy_file, name, "__probe__") {
                    Ok(_) | Err(lex_syntax::PackageError::ModuleNotFound { .. }) => {
                        println!("ok");
                    }
                    Err(e) => {
                        println!("FAILED");
                        eprintln!("    {e}");
                        errors.push(format!("{name}: {e}"));
                    }
                }
            }
        }
    }

    if errors.is_empty() {
        println!("all {} dependency/dependencies installed", manifest.dependencies.len());
        Ok(())
    } else {
        bail!("{} dependency/dependencies failed to install", errors.len())
    }
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
                println!("  {name}  path     = {path}"),
            lex_syntax::workspace::Dependency::Git { git, branch, tag, rev } => {
                let pin = branch.as_deref().map(|b| format!(" branch={b}"))
                    .or_else(|| tag.as_deref().map(|t| format!(" tag={t}")))
                    .or_else(|| rev.as_deref().map(|r| format!(" rev={}", &r[..r.len().min(12)])))
                    .unwrap_or_else(|| " (unpinned — consider adding tag or rev)".into());
                println!("  {name}  git      = {git}{pin}");
            }
            lex_syntax::workspace::Dependency::Registry { registry, version } =>
                println!("  {name}  registry = {registry}  version = {version}"),
        }
    }
    Ok(())
}

// ── lex pkg publish ───────────────────────────────────────────────────────────

/// Pack `lex.toml` + `src/**` into a gzipped tar archive in memory.
fn build_archive(toml_dir: &std::path::Path) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let gz = flate2::write::GzEncoder::new(buf, flate2::Compression::default());
    let mut ar = tar::Builder::new(gz);

    // Include lex.toml at archive root.
    let toml_path = toml_dir.join("lex.toml");
    ar.append_path_with_name(&toml_path, "lex.toml")?;

    // Include every file under src/.
    let src_dir = toml_dir.join("src");
    if src_dir.exists() {
        ar.append_dir_all("src", &src_dir)?;
    }

    let gz = ar.into_inner()?;
    Ok(gz.finish()?)
}

fn cmd_publish(args: &[String]) -> Result<()> {
    let mut registry_arg: Option<String> = None;
    let mut token_arg:    Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--registry" => {
                i += 1;
                registry_arg = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--registry requires a value")
                })?);
            }
            "--token" => {
                i += 1;
                token_arg = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--token requires a value")
                })?);
            }
            other => bail!("unknown flag `{other}`"),
        }
        i += 1;
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let (toml_path, toml_dir) = lex_syntax::find_manifest(&cwd)
        .ok_or_else(|| anyhow::anyhow!("no lex.toml found (run `lex pkg init` to create one)"))?;

    let manifest = lex_syntax::Manifest::load(&toml_path)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let pkg = manifest.package.as_ref()
        .ok_or_else(|| anyhow::anyhow!("lex.toml must have a [package] section"))?;

    let registry = registry_arg
        .or_else(|| pkg.registry.clone())
        .ok_or_else(|| anyhow::anyhow!(
            "no registry URL: pass --registry <url> or set `registry` in [package]"
        ))?;

    let token = token_arg
        .or_else(|| std::env::var("LEX_PUBLISH_TOKEN").ok())
        .ok_or_else(|| anyhow::anyhow!(
            "no publish token: pass --token <jwt> or set LEX_PUBLISH_TOKEN"
        ))?;

    println!("publishing {}@{} to {} ...", pkg.name, pkg.version, registry);

    let archive = build_archive(&toml_dir)
        .map_err(|e| anyhow::anyhow!("building archive: {e}"))?;

    let url = format!("{}/v1/pkg/publish", registry.trim_end_matches('/'));
    let response = ureq::post(&url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .send(&archive[..])
        .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;

    let status = response.status().as_u16();
    let body: serde_json::Value = response
        .into_body()
        .read_json()
        .map_err(|e| anyhow::anyhow!("reading response: {e}"))?;

    if status != 200 {
        bail!(
            "publish failed (HTTP {status}): {}",
            body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown error")
        );
    }

    let head_op = body.get("head_op").and_then(|v| v.as_str()).unwrap_or("(none)");
    println!("published  head_op = {head_op}");
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
