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
use std::path::Path;

use crate::capsule_contract::{
    ArtifactRef, CapabilityContract, Keyring, SignedContract,
};

pub fn cmd_pkg(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("init")    => cmd_init(),
        Some("add")     => cmd_add(&args[1..]),
        Some("list")    => cmd_list(),
        Some("install") => cmd_install(),
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
