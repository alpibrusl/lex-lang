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
//!       [--sign <key>] [--requires <grant.json>] [--egress h,h] [--contract-out <file>]
//!                                                      — also emit a signed capability
//!                                                        contract (the lex-os capsule
//!                                                        format) binding the published
//!                                                        bytes to the grant it needs
//!   lex pkg verify --archive <tar> --contract <c.json> [--trusted-keys <keyring.json>]
//!                                                      — verify a package against its
//!                                                        signed contract (signature +
//!                                                        content hash + signer trust)

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::capsule_contract::{
    ArtifactRef, CapabilityContract, Keyring, SignedContract,
};

pub fn cmd_pkg(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("init")    => cmd_init(),
        Some("add")     => cmd_add(&args[1..]),
        Some("list")    => cmd_list(),
        Some("install") => cmd_install(&args[1..]),
        Some("publish") => cmd_publish(&args[1..]),
        Some("verify")  => cmd_verify(&args[1..]),
        Some(other)     => bail!("unknown pkg subcommand `{other}`; try: init, add, install, list, publish, verify"),
        None            => bail!("usage: lex pkg <init|add|install|list|publish|verify>"),
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

/// Resolve and install the **full transitive closure** of dependencies
/// declared in the nearest lex.toml (#634).
///
/// Algorithm: BFS starting from the top-level manifest. After each
/// package is successfully cloned its own `lex.toml` is read and its
/// deps are enqueued (if not already seen). A `seen` map (name →
/// source-key) terminates cycles and detects version conflicts.
///
/// Conflict policy: two packages in the closure requiring the same dep
/// name under different sources (different git URL / ref / path) is an
/// error — the flat `LEX_PACKAGES_DIR` layout can only hold one copy.
///
/// For registry dependencies: fetch the published **signed contract** and
/// verify the archive against it before trusting it — `--trusted-keys`
/// additionally pins the signer, and `--require-contracts` refuses any
/// registry dep the registry serves unsigned.
fn cmd_install(args: &[String]) -> Result<()> {
    use std::collections::{HashMap, VecDeque};

    let mut trusted_keys: Option<String> = None;
    let mut require_contracts = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--trusted-keys" => {
                i += 1;
                trusted_keys = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--trusted-keys requires a path")
                })?);
            }
            "--require-contracts" => require_contracts = true,
            other => bail!("unknown flag `{other}`"),
        }
        i += 1;
    }
    let keyring = match &trusted_keys {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading keyring {path}"))?;
            Some(serde_json::from_str::<Keyring>(&raw).with_context(|| format!("parsing keyring {path}"))?)
        }
        None => None,
    };

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

    // BFS queue: (dep_name, dir_of_declaring_lex_toml, is_direct_dep)
    let mut queue: VecDeque<(String, PathBuf, bool)> = VecDeque::new();
    // seen: dep_name → flat-layout identity — detects cycles and version conflicts.
    let mut seen: HashMap<String, DepIdentity> = HashMap::new();

    for (name, dep) in &manifest.dependencies {
        seen.insert(name.clone(), dep_identity(dep, &toml_dir));
        queue.push_back((name.clone(), toml_dir.clone(), true));
    }

    let mut errors: Vec<String> = Vec::new();
    let mut total = 0usize;

    while let Some((name, importer_dir, direct)) = queue.pop_front() {
        let dummy = importer_dir.join("__install_probe__.lex");

        // Read dep from declaring manifest (owned) for display + registry contract check.
        let dep_owned: Option<lex_syntax::workspace::Dependency> =
            lex_syntax::Manifest::load(&importer_dir.join("lex.toml"))
                .ok()
                .and_then(|mut m| m.dependencies.remove(&name));

        let dep_display = match &dep_owned {
            Some(lex_syntax::workspace::Dependency::Git { git, branch, tag, rev }) => {
                let ref_desc = branch.as_deref().map(|b| format!(" branch={b}"))
                    .or_else(|| tag.as_deref().map(|t| format!(" tag={t}")))
                    .or_else(|| rev.as_deref().map(|r| format!(" rev={}", &r[..r.len().min(12)])))
                    .unwrap_or_default();
                format!("{}{}", git, ref_desc)
            }
            Some(lex_syntax::workspace::Dependency::Path { path }) => path.clone(),
            Some(lex_syntax::workspace::Dependency::Registry { registry, version }) =>
                format!("{registry}@{version}"),
            None => "?".into(),
        };
        let transitive_tag = if direct { "" } else { "  [transitive]" };
        print!("  {name}  {dep_display}{transitive_tag} ... ");
        std::io::Write::flush(&mut std::io::stdout()).ok();

        // For registry deps: verify the published contract before installing.
        if let Some(lex_syntax::workspace::Dependency::Registry { registry, version }) = &dep_owned {
            match verify_registry_dep(registry, &name, version, keyring.as_ref()) {
                Ok(DepVerification::Verified { signer, signer_trusted, .. }) => {
                    let trust = if signer_trusted {
                        " (signer trusted)".to_string()
                    } else if keyring.is_some() {
                        String::new()
                    } else {
                        " (signer not pinned — pass --trusted-keys)".to_string()
                    };
                    print!("contract verified, signer {:.12}…{trust} ... ", signer);
                    std::io::Write::flush(&mut std::io::stdout()).ok();
                }
                Ok(DepVerification::NoContract) => {
                    if require_contracts {
                        println!("REFUSED");
                        let msg = format!(
                            "{name}: registry served no contract and --require-contracts is set"
                        );
                        eprintln!("    {msg}");
                        errors.push(msg);
                        continue;
                    }
                    eprint!("⚠ no contract (unsigned) ... ");
                }
                Err(e) => {
                    println!("REFUSED");
                    eprintln!("    {name}: contract verification failed: {e}");
                    errors.push(format!("{name}: {e}"));
                    continue;
                }
            }
        }

        match lex_syntax::workspace::resolve_package_import(&dummy, &name, "__probe__") {
            Ok(_) => {
                println!("ok");
                total += 1;
            }
            Err(lex_syntax::PackageError::ModuleNotFound { pkg_root, .. }) => {
                println!("ok");
                total += 1;
                // Read the installed package's own lex.toml and enqueue its deps.
                let pkg_dir = std::path::Path::new(&pkg_root);
                let pkg_toml = pkg_dir.join("lex.toml");
                if pkg_toml.exists() {
                    match lex_syntax::Manifest::load(&pkg_toml) {
                        Ok(dep_manifest) => {
                            for (trans_name, trans_dep) in &dep_manifest.dependencies {
                                let new_id = dep_identity(trans_dep, pkg_dir);
                                match seen.get(trans_name) {
                                    Some(existing) => {
                                        if let Some(msg) =
                                            conflict_message(trans_name, existing, &new_id, &name)
                                        {
                                            eprintln!("  error: {msg}");
                                            errors.push(msg);
                                        } else if matches!(existing, DepIdentity::Alias)
                                            && matches!(new_id, DepIdentity::Concrete(_))
                                        {
                                            // A concrete source supersedes a prior path
                                            // alias for the same slot: record it and queue
                                            // the install so the slot gets populated.
                                            seen.insert(trans_name.clone(), new_id);
                                            queue.push_back((
                                                trans_name.clone(),
                                                PathBuf::from(&pkg_root),
                                                false,
                                            ));
                                        }
                                        // else: same source, or a harmless alias of an
                                        // already-present slot — skip.
                                    }
                                    None => {
                                        seen.insert(trans_name.clone(), new_id);
                                        queue.push_back((
                                            trans_name.clone(),
                                            PathBuf::from(&pkg_root),
                                            false,
                                        ));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "  warning: could not read {}: {e} \
                                 (transitive deps of `{name}` may be incomplete)",
                                pkg_toml.display()
                            );
                        }
                    }
                }
            }
            Err(e) => {
                println!("FAILED");
                eprintln!("    {e}");
                errors.push(format!("{name}: {e}"));
            }
        }
    }

    if errors.is_empty() {
        println!("installed {} package(s) (full transitive closure)", total);
        Ok(())
    } else {
        bail!("{} error(s) during install", errors.len())
    }
}

/// A dependency's identity for flat-layout conflict detection.
///
/// The flat `LEX_PACKAGES_DIR` layout holds a single copy per package name, so
/// a conflict is "two requirements would fill the same slot with different
/// bytes" — not "two declarations spell their source differently". A git dep
/// and a sibling-`path` dep (`../name`, the standard inter-package reference)
/// both resolve to the *same* cache slot, so they must not be treated as a
/// conflict.
#[derive(Clone, PartialEq, Eq)]
enum DepIdentity {
    /// A source that *populates* the package's cache slot with specific bytes:
    /// a git repo, a registry release, or a local path pointing *outside* the
    /// cache. Two differing concrete identities for one name genuinely conflict.
    Concrete(String),
    /// A path dependency resolving to a sibling already inside the package
    /// cache. It introduces no new bytes — it aliases whatever fills that slot
    /// — so it never conflicts with the concrete source that installs there.
    Alias,
}

/// Classify a dependency for conflict detection. Git/registry deps are always
/// concrete; a `path` dep is an [`DepIdentity::Alias`] when it resolves into the
/// package cache, otherwise a concrete local-path source.
fn dep_identity(dep: &lex_syntax::workspace::Dependency, base_dir: &Path) -> DepIdentity {
    use lex_syntax::workspace::Dependency;
    match dep {
        Dependency::Git { .. } | Dependency::Registry { .. } => {
            DepIdentity::Concrete(dep_source_key(dep, base_dir))
        }
        Dependency::Path { path } => {
            let resolved = normalize_path(&base_dir.join(path));
            if path_in_cache(&resolved) {
                DepIdentity::Alias
            } else {
                DepIdentity::Concrete(format!("path:{}", resolved.display()))
            }
        }
    }
}

/// Returns `Some(message)` when two flat-layout requirements for the same name
/// genuinely conflict, or `None` when they can coexist. Only two *concrete*
/// sources with differing identities collide; an alias defers to whatever fills
/// the slot.
fn conflict_message(
    name: &str,
    existing: &DepIdentity,
    incoming: &DepIdentity,
    requiring_pkg: &str,
) -> Option<String> {
    match (existing, incoming) {
        (DepIdentity::Concrete(a), DepIdentity::Concrete(b)) if a != b => Some(format!(
            "version conflict: `{name}` is required as \"{a}\" (already seen) but \
             `{requiring_pkg}` requires \"{b}\" — flat layout can only hold one copy"
        )),
        _ => None,
    }
}

/// Canonical string key for a git/registry dependency's bytes-source.
fn dep_source_key(dep: &lex_syntax::workspace::Dependency, base_dir: &Path) -> String {
    use lex_syntax::workspace::Dependency;
    match dep {
        Dependency::Git { git, branch, tag, rev } => {
            let ref_part = branch.as_deref().map(|b| format!("@branch:{b}"))
                .or_else(|| tag.as_deref().map(|t| format!("@tag:{t}")))
                .or_else(|| rev.as_deref().map(|r| format!("@rev:{r}")))
                .unwrap_or_default();
            format!("git:{git}{ref_part}")
        }
        Dependency::Path { path } => format!("path:{}", base_dir.join(path).display()),
        Dependency::Registry { registry, version } => format!("registry:{registry}@{version}"),
    }
}

/// Resolve a path for cache-membership comparison: canonicalize if it exists,
/// otherwise fold `.`/`..` lexically so `…/lex-web/../lex-orm` reduces to
/// `…/lex-orm` even when the target isn't on disk yet.
fn normalize_path(p: &Path) -> PathBuf {
    if let Ok(canonical) = p.canonicalize() {
        return canonical;
    }
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => { out.pop(); }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True if `resolved` lives inside the flat package cache root — i.e. it is a
/// sibling reference to another already-installed package, not external bytes.
fn path_in_cache(resolved: &Path) -> bool {
    match lex_syntax::workspace::packages_cache_root() {
        Some(root) => {
            let root = root.canonicalize().unwrap_or(root);
            resolved.starts_with(&root)
        }
        None => false,
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
    // Provenance: when --sign is given, emit a signed capability contract
    // (the lex-os capsule format) alongside / instead of uploading.
    let mut sign_arg:     Option<String> = None;
    let mut requires_arg: Option<String> = None;
    let mut egress_arg:   Vec<String>    = Vec::new();
    let mut contract_out: Option<String> = None;
    let mut archive_out:  Option<String> = None;
    // Derive the required grant from the entrypoint's typed effects instead of
    // declaring it with --requires.
    let mut derive_grant = false;
    let mut entrypoint:   Option<String> = None;
    let mut no_upload = false;
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
            "--sign" => {
                i += 1;
                sign_arg = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--sign requires a key (64-hex secret, or @path / env LEX_SIGNING_KEY)")
                })?);
            }
            "--requires" => {
                i += 1;
                requires_arg = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--requires requires a path to a grant JSON file")
                })?);
            }
            "--egress" => {
                i += 1;
                let v = args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--egress requires a comma-separated host list")
                })?;
                egress_arg.extend(v.split(',').filter(|s| !s.is_empty()).map(String::from));
            }
            "--contract-out" => {
                i += 1;
                contract_out = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--contract-out requires a path")
                })?);
            }
            "--archive-out" => {
                i += 1;
                archive_out = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--archive-out requires a path")
                })?);
            }
            "--derive-grant" => derive_grant = true,
            "--entrypoint" => {
                i += 1;
                entrypoint = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--entrypoint requires a path (relative to lex.toml, default src/main.lex)")
                })?);
            }
            "--no-upload" => no_upload = true,
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

    // Build the archive once: the same bytes are hashed for the contract and
    // uploaded to the registry, so the content_hash binds what installs.
    let archive = build_archive(&toml_dir)
        .map_err(|e| anyhow::anyhow!("building archive: {e}"))?;
    let content_hash = crate::capsule_contract::hash_artifact_bytes(&archive);

    // Emit the exact bytes the content_hash binds, so a consumer can verify and
    // `lex-os capsule install --artifact <archive>` can run them.
    if let Some(path) = &archive_out {
        std::fs::write(path, &archive)
            .with_context(|| format!("writing archive to {path}"))?;
    }

    // ── Provenance: emit a signed capability contract ──────────────────────
    if let Some(key_spec) = &sign_arg {
        if derive_grant && requires_arg.is_some() {
            bail!("--derive-grant and --requires are mutually exclusive (derive from code, or declare)");
        }
        // The grant the contract requires: derived from the entrypoint's typed
        // effects, or declared via --requires.
        let (requires, mut egress) = if derive_grant {
            let entry_rel = entrypoint.as_deref().unwrap_or("src/main.lex");
            let entry_path = toml_dir.join(entry_rel);
            let source = std::fs::read_to_string(&entry_path).with_context(|| {
                format!("reading entrypoint {} for --derive-grant", entry_path.display())
            })?;
            let (grant, derived_egress) =
                crate::capsule_contract::derive_grant_from_source(&source)
                    .map_err(|e| anyhow::anyhow!("deriving grant from {entry_rel}: {e}"))?;
            println!(
                "derived grant from {entry_rel}:  fs={}  net={}  exec={}",
                grant.filesystem.as_str(),
                grant.network.as_str(),
                grant.exec.as_str(),
            );
            (grant, derived_egress)
        } else {
            let requires_path = requires_arg.as_ref().ok_or_else(|| anyhow::anyhow!(
                "--sign needs --requires <grant.json> or --derive-grant (the capability the package needs)"
            ))?;
            (load_grant(requires_path)?, Vec::new())
        };
        // Any explicit --egress hosts are unioned in (e.g. for bare `[net]`).
        for h in &egress_arg {
            if !egress.contains(h) {
                egress.push(h.clone());
            }
        }
        egress.sort();
        egress.dedup();
        let keypair = resolve_pkg_signing_key(key_spec)?;

        let contract = CapabilityContract {
            artifact: ArtifactRef {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                content_hash: content_hash.clone(),
            },
            requires,
            egress,
        };
        let contract_id = contract.content_id();
        let signed = contract.sign(&keypair);
        let json = serde_json::to_string_pretty(&signed)
            .map_err(|e| anyhow::anyhow!("serializing contract: {e}"))?;

        let out_path = contract_out
            .clone()
            .unwrap_or_else(|| format!("{}-{}.contract.json", pkg.name, pkg.version));
        std::fs::write(&out_path, format!("{json}\n"))
            .with_context(|| format!("writing contract to {out_path}"))?;
        println!(
            "signed contract  {}@{}  contract_id={contract_id}  content_hash={content_hash}  signer={}",
            pkg.name,
            pkg.version,
            signed.signer,
        );
        println!("  -> {out_path}  (install with: lex-os capsule install --contract {out_path} --artifact <archive>)");
    } else if contract_out.is_some()
        || requires_arg.is_some()
        || !egress_arg.is_empty()
        || derive_grant
        || entrypoint.is_some()
    {
        bail!("--requires / --derive-grant / --entrypoint / --egress / --contract-out only apply with --sign");
    }

    // ── Registry upload ────────────────────────────────────────────────────
    // Resolve the registry/token; if a contract was emitted and no registry is
    // configured, that's a complete local publish — don't force an upload.
    let registry = registry_arg.or_else(|| pkg.registry.clone());
    let token = token_arg.or_else(|| std::env::var("LEX_PUBLISH_TOKEN").ok());

    if no_upload {
        return Ok(());
    }
    match (registry, token) {
        (Some(registry), Some(token)) => {
            println!("publishing {}@{} to {} ...", pkg.name, pkg.version, registry);
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
        // Contract emitted but no registry: a complete local publish.
        _ if sign_arg.is_some() => Ok(()),
        _ => bail!(
            "no registry URL: pass --registry <url> or set `registry` in [package] \
             (or use --sign … to emit a contract locally without uploading)"
        ),
    }
}

// ── lex pkg verify ────────────────────────────────────────────────────────────

/// Verify a published package against its signed capability contract: the
/// signature binds the contract to its declared signer, the archive bytes hash
/// to the contract's `content_hash`, and — with `--trusted-keys` — the signer
/// is one the consumer pinned. The same three gates `lex-os capsule install`
/// applies, available to the package manager before a dependency is trusted.
fn cmd_verify(args: &[String]) -> Result<()> {
    let mut archive_path: Option<String> = None;
    let mut contract_path: Option<String> = None;
    let mut trusted_keys: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--archive" => {
                i += 1;
                archive_path = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--archive requires a path")
                })?);
            }
            "--contract" => {
                i += 1;
                contract_path = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--contract requires a path")
                })?);
            }
            "--trusted-keys" => {
                i += 1;
                trusted_keys = Some(args.get(i).cloned().ok_or_else(|| {
                    anyhow::anyhow!("--trusted-keys requires a path")
                })?);
            }
            other => bail!("unknown flag `{other}`"),
        }
        i += 1;
    }
    let archive_path = archive_path.ok_or_else(|| anyhow::anyhow!(
        "usage: lex pkg verify --archive <tar> --contract <c.json> [--trusted-keys <keyring.json>]"
    ))?;
    let contract_path = contract_path.ok_or_else(|| anyhow::anyhow!(
        "usage: lex pkg verify --archive <tar> --contract <c.json> [--trusted-keys <keyring.json>]"
    ))?;

    let contract_raw = std::fs::read_to_string(&contract_path)
        .with_context(|| format!("reading contract {contract_path}"))?;
    let signed: SignedContract = serde_json::from_str(&contract_raw)
        .with_context(|| format!("parsing contract {contract_path}"))?;
    let archive = std::fs::read(&archive_path)
        .with_context(|| format!("reading archive {archive_path}"))?;

    // 1. Authenticity: the signature binds the contract to its signer.
    let signer = signed.verify().map_err(|e| anyhow::anyhow!("authenticity: {e}"))?;
    // 2. Integrity: the bytes are the ones the contract was signed over.
    signed.matches_artifact(&archive).map_err(|e| anyhow::anyhow!("integrity: {e}"))?;
    // 3. Authorization: the signer is trusted (if a keyring was supplied).
    let trust_checked = match &trusted_keys {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading keyring {path}"))?;
            let keyring: Keyring = serde_json::from_str(&raw)
                .with_context(|| format!("parsing keyring {path}"))?;
            if !keyring.trusts(&signer) {
                bail!("authorization: signer {signer} is not in the trusted keyring {path}");
            }
            true
        }
        None => {
            eprintln!(
                "⚠  signer NOT checked against a trusted keyring — any valid signature is \
                 accepted. Pass --trusted-keys <keyring.json> to pin publishers."
            );
            false
        }
    };

    let a = &signed.contract.artifact;
    println!(
        "verified  {}@{}  content_hash={}  signer={signer}  signer_trust_checked={trust_checked}",
        a.name, a.version, a.content_hash,
    );
    Ok(())
}

/// Load a `lex_types::trust::Grant` from a JSON file (the `--requires` shape:
/// `{"filesystem":"ReadOnly","network":"Allowlist","exec":"None"}`).
fn load_grant(path: &str) -> Result<lex_types::trust::Grant> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading grant file {path}"))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parsing grant {path} (expected {{\"filesystem\":…,\"network\":…,\"exec\":…}})"))
}

/// Resolve a signing key for `lex pkg publish --sign`. Accepts a 64-hex secret
/// directly, `@<path>` to read it from a file, or falls back to
/// `LEX_SIGNING_KEY` when the literal `env` is passed — mirroring how
/// `lex publish` resolves its signer.
fn resolve_pkg_signing_key(spec: &str) -> Result<lex_vcs::Keypair> {
    let hex = if let Some(path) = spec.strip_prefix('@') {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading signing key from {path}"))?
            .trim()
            .to_string()
    } else if spec == "env" {
        std::env::var("LEX_SIGNING_KEY")
            .map_err(|_| anyhow::anyhow!("--sign env requires LEX_SIGNING_KEY to be set"))?
    } else {
        spec.to_string()
    };
    lex_vcs::Keypair::from_secret_hex(&hex)
        .map_err(|e| anyhow::anyhow!("invalid signing key: {e}"))
}

/// Outcome of verifying a registry dependency against its published contract.
enum DepVerification {
    /// The registry served a contract; its signature binds it to `signer` and
    /// the archive bytes hash to the contract's `content_hash`. `signer_trusted`
    /// is set when a `--trusted-keys` keyring was supplied and lists the signer.
    Verified {
        signer: String,
        signer_trusted: bool,
    },
    /// The registry served no contract (HTTP 404) — an unsigned dependency.
    NoContract,
}

/// Fetch `{registry}/v1/pkg/{name}/{version}/{contract,archive}` and verify the
/// archive against the signed contract: authenticity (the signature binds the
/// contract to its signer), integrity (the archive bytes hash to the contract's
/// `content_hash`), and — when `trusted` is supplied — authorization (the signer
/// is pinned). The same three gates `lex-os capsule install` applies, now at
/// `lex pkg install` time, against the same `lex pkg publish`-emitted contract.
fn verify_registry_dep(
    registry: &str,
    name: &str,
    version: &str,
    trusted: Option<&Keyring>,
) -> Result<DepVerification> {
    let base = registry.trim_end_matches('/');
    let contract_url = format!("{base}/v1/pkg/{name}/{version}/contract");
    let signed: SignedContract = match ureq::get(&contract_url).call() {
        Ok(resp) => {
            let body = resp
                .into_body()
                .read_to_string()
                .map_err(|e| anyhow::anyhow!("reading contract: {e}"))?;
            serde_json::from_str(&body)
                .with_context(|| format!("parsing contract from {contract_url}"))?
        }
        // A registry that doesn't publish a contract for this version.
        Err(ureq::Error::StatusCode(404)) => return Ok(DepVerification::NoContract),
        Err(e) => bail!("GET {contract_url}: {e}"),
    };

    let archive_url = format!("{base}/v1/pkg/{name}/{version}/archive");
    let archive = ureq::get(&archive_url)
        .call()
        .map_err(|e| anyhow::anyhow!("GET {archive_url}: {e}"))?
        .into_body()
        .read_to_vec()
        .map_err(|e| anyhow::anyhow!("reading archive: {e}"))?;

    // 1. Authenticity, 2. Integrity (same primitives as `lex pkg verify`).
    let signer = signed
        .verify()
        .map_err(|e| anyhow::anyhow!("authenticity: {e}"))?;
    signed
        .matches_artifact(&archive)
        .map_err(|e| anyhow::anyhow!("integrity: {e}"))?;
    // 3. Authorization (only when a keyring pins publishers).
    let signer_trusted = match trusted {
        Some(k) => {
            if !k.trusts(&signer) {
                bail!("authorization: signer {signer} is not in the trusted keyring");
            }
            true
        }
        None => false,
    };
    Ok(DepVerification::Verified {
        signer,
        signer_trusted,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use lex_syntax::workspace::Dependency;

    fn git(url: &str) -> Dependency {
        Dependency::Git { git: url.into(), branch: None, tag: None, rev: None }
    }
    fn git_tag(url: &str, tag: &str) -> Dependency {
        Dependency::Git { git: url.into(), branch: None, tag: Some(tag.into()), rev: None }
    }
    fn path(p: &str) -> Dependency {
        Dependency::Path { path: p.into() }
    }

    /// All conflict-detection scenarios from issue #637, exercised through the
    /// pure classification logic. Runs in one test so the single shared
    /// `LEX_PACKAGES_DIR` env var is not raced by parallel tests.
    #[test]
    fn flat_layout_conflict_detection() {
        let cache = tempfile::tempdir().unwrap();
        let cache_root = cache.path().to_path_buf();
        // The cache must exist on disk for `path_in_cache` to canonicalize it.
        let web_dir = cache_root.join("lex-web");
        let orm_dir = cache_root.join("lex-orm");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::create_dir_all(&orm_dir).unwrap();
        std::env::set_var("LEX_PACKAGES_DIR", &cache_root);

        // The top manifest declares lex-orm as a git dep → concrete.
        let top = git("https://github.com/alpibrusl/lex-orm");
        let top_id = dep_identity(&top, &cache_root);
        assert!(matches!(top_id, DepIdentity::Concrete(_)));

        // lex-web's own manifest references lex-orm as a sibling path `../lex-orm`.
        // Resolved from inside the cache it is an alias of the same slot — #637.
        let sibling = path("../lex-orm");
        let sibling_id = dep_identity(&sibling, &web_dir);
        assert!(matches!(sibling_id, DepIdentity::Alias));

        // Acceptance 1: git dep + sibling-path dep for the same name → no conflict.
        assert!(
            conflict_message("lex-orm", &top_id, &sibling_id, "lex-web").is_none(),
            "git dep and sibling-path dep must not falsely conflict"
        );

        // Acceptance 3a: two different git URLs for the same name → real conflict.
        let other_url = git("https://github.com/someone-else/lex-orm");
        let other_id = dep_identity(&other_url, &cache_root);
        assert!(
            conflict_message("lex-orm", &top_id, &other_id, "lex-web").is_some(),
            "different git URLs for one name must still conflict"
        );

        // Acceptance 3b: two different git refs for the same URL → real conflict.
        let pinned = git_tag("https://github.com/alpibrusl/lex-orm", "v1.0.0");
        let pinned_id = dep_identity(&pinned, &cache_root);
        assert!(
            conflict_message("lex-orm", &top_id, &pinned_id, "lex-web").is_some(),
            "different git refs for one name must still conflict"
        );

        // A path pointing *outside* the cache is concrete (external bytes), so it
        // conflicts with the git source rather than aliasing it.
        let external = path("../../somewhere-else/lex-orm");
        let external_id = dep_identity(&external, cache.path().parent().unwrap());
        assert!(matches!(external_id, DepIdentity::Concrete(_)));
        assert!(
            conflict_message("lex-orm", &top_id, &external_id, "lex-web").is_some(),
            "an out-of-cache path with different bytes must conflict"
        );

        std::env::remove_var("LEX_PACKAGES_DIR");
    }
}
