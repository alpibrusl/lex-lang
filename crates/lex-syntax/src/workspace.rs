//! `lex.toml` manifest parsing and package resolution.
//!
//! A `lex.toml` file marks a project root and declares its package
//! dependencies. Import paths of the form `"pkg-name/module"` are
//! resolved against this file.
//!
//! ## File format
//!
//! ```toml
//! [package]
//! name = "lex-web"
//! version = "0.1.0"
//!
//! [dependencies]
//! lex-schema = { path = "../lex-schema" }
//! # or:
//! lex-schema = { git = "https://github.com/alpibrusl/lex-schema" }
//! # or:
//! lex-schema = { registry = "https://lexhub.alpibru.com", version = "0.9.2" }
//! ```
//!
//! ## Module resolution
//!
//! `import "lex-schema/validate" as v` splits into `pkg = "lex-schema"`,
//! `module = "validate"`. The loader:
//!
//! 1. Walks up from the importing file to find the nearest `lex.toml`.
//! 2. Looks up `lex-schema` in `[dependencies]`.
//! 3. For `path =`: resolves `{dep_path}/src/validate.lex`; falls back to
//!    `{dep_path}/validate.lex` if `src/` doesn't exist.
//! 4. For `git =`: clones the repo into `~/.lex/packages/lex-schema/`
//!    (once; subsequent loads hit the cache), then resolves the same way.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ── Manifest types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub package: Option<PackageMeta>,
    #[serde(default)]
    pub dependencies: HashMap<String, Dependency>,
}

#[derive(Debug, Deserialize)]
pub struct PackageMeta {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Default registry URL for `lex pkg publish` when `--registry` is not supplied.
    #[serde(default)]
    pub registry: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Dependency {
    Path     { path: String },
    Git      { git:  String },
    Registry { registry: String, version: String },
}

impl Manifest {
    pub fn load(toml_path: &Path) -> Result<Self, String> {
        let src = std::fs::read_to_string(toml_path)
            .map_err(|e| format!("reading {}: {e}", toml_path.display()))?;
        toml::from_str(&src)
            .map_err(|e| format!("parsing {}: {e}", toml_path.display()))
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Walk up from `start` (a file or directory) looking for `lex.toml`.
/// Returns `(toml_path, toml_dir)` for the nearest ancestor that has one.
pub fn find_manifest(start: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        let candidate = dir.join("lex.toml");
        if candidate.exists() {
            return Some((candidate, dir));
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p.to_path_buf(),
            _ => return None,
        }
    }
}

// ── Resolution ────────────────────────────────────────────────────────────────

/// Resolve `pkg_name/module_path` to a `.lex` file on disk.
///
/// `importer` is the file that contains the import statement; it's used
/// to locate the nearest `lex.toml`.
pub fn resolve_package_import(
    importer: &Path,
    pkg_name: &str,
    module_path: &str,
) -> Result<PathBuf, PackageError> {
    let (toml_path, toml_dir) = find_manifest(importer).ok_or_else(|| {
        PackageError::NoManifest {
            reference: format!("{pkg_name}/{module_path}"),
            searched_from: importer.display().to_string(),
        }
    })?;

    let manifest = Manifest::load(&toml_path)
        .map_err(|e| PackageError::ManifestParse { path: toml_path.display().to_string(), detail: e })?;

    let dep = manifest.dependencies.get(pkg_name).ok_or_else(|| {
        PackageError::UnknownPackage {
            name: pkg_name.to_string(),
            manifest: toml_path.display().to_string(),
        }
    })?;

    let pkg_root = match dep {
        Dependency::Path { path } => {
            let raw = toml_dir.join(path);
            raw.canonicalize().map_err(|e| PackageError::Io {
                path: raw.display().to_string(),
                detail: e.to_string(),
            })?
        }
        Dependency::Git { git } => git_ensure_cached(pkg_name, git)?,
        Dependency::Registry { registry, version } => {
            registry_ensure_cached(pkg_name, registry, version)?
        }
    };

    find_module_file(&pkg_root, module_path).ok_or_else(|| PackageError::ModuleNotFound {
        pkg: pkg_name.to_string(),
        module: module_path.to_string(),
        pkg_root: pkg_root.display().to_string(),
    })
}

/// Look for `{module_path}.lex` inside a package root, checking `src/`
/// first then the root itself.
fn find_module_file(pkg_root: &Path, module_path: &str) -> Option<PathBuf> {
    let rel = PathBuf::from(module_path).with_extension("lex");
    let in_src = pkg_root.join("src").join(&rel);
    if in_src.exists() {
        return Some(in_src);
    }
    let at_root = pkg_root.join(&rel);
    if at_root.exists() {
        return Some(at_root);
    }
    None
}

// ── Git cache ─────────────────────────────────────────────────────────────────

/// Return the local cache directory for `pkg_name`, cloning from `url`
/// if it isn't there yet.
///
/// Cache root: `$LEX_PACKAGES_DIR` if set, otherwise `~/.lex/packages/`.
fn git_ensure_cached(pkg_name: &str, url: &str) -> Result<PathBuf, PackageError> {
    let cache_root = packages_cache_dir()?;
    let pkg_dir = cache_root.join(pkg_name);
    if pkg_dir.exists() {
        return Ok(pkg_dir);
    }
    std::fs::create_dir_all(&cache_root).map_err(|e| PackageError::Io {
        path: cache_root.display().to_string(),
        detail: e.to_string(),
    })?;
    let status = std::process::Command::new("git")
        .args(["clone", "--depth=1", url, pkg_dir.to_str().unwrap_or(pkg_name)])
        .status()
        .map_err(|e| PackageError::GitFailed {
            url: url.to_string(),
            detail: format!("could not run `git`: {e}"),
        })?;
    if !status.success() {
        return Err(PackageError::GitFailed {
            url: url.to_string(),
            detail: format!("`git clone` exited with {status}"),
        });
    }
    pkg_dir.canonicalize().map_err(|e| PackageError::Io {
        path: pkg_dir.display().to_string(),
        detail: e.to_string(),
    })
}

/// Download a registry package archive and extract it to the local cache.
///
/// Cache path: `$LEX_PACKAGES_DIR/{name}-{version}/` (versioned to avoid
/// collisions with git-cached packages at `{name}/`).
///
/// Download URL: `{registry}/v1/pkg/{name}/{version}/archive`
fn registry_ensure_cached(
    pkg_name: &str,
    registry: &str,
    version: &str,
) -> Result<PathBuf, PackageError> {
    let cache_root = packages_cache_dir()?;
    // Registry packages are cached under `{name}-{version}` to keep multiple
    // versions side-by-side and separate from git-cached directories.
    let pkg_dir = cache_root.join(format!("{pkg_name}-{version}"));
    if pkg_dir.exists() {
        return Ok(pkg_dir);
    }
    std::fs::create_dir_all(&cache_root).map_err(|e| PackageError::Io {
        path: cache_root.display().to_string(),
        detail: e.to_string(),
    })?;

    let url = format!(
        "{}/v1/pkg/{}/{}/archive",
        registry.trim_end_matches('/'),
        pkg_name,
        version,
    );
    let response = ureq::get(&url).call().map_err(|e| PackageError::RegistryFailed {
        name: pkg_name.to_string(),
        registry: registry.to_string(),
        version: version.to_string(),
        detail: format!("GET {url}: {e}"),
    })?;
    if response.status() != 200 {
        return Err(PackageError::RegistryFailed {
            name: pkg_name.to_string(),
            registry: registry.to_string(),
            version: version.to_string(),
            detail: format!("GET {url} returned HTTP {}", response.status()),
        });
    }

    let archive_bytes = response
        .into_body()
        .read_to_vec()
        .map_err(|e| PackageError::RegistryFailed {
            name: pkg_name.to_string(),
            registry: registry.to_string(),
            version: version.to_string(),
            detail: format!("reading response body: {e}"),
        })?;

    let gz = flate2::read::GzDecoder::new(std::io::Cursor::new(&archive_bytes));
    let mut ar = tar::Archive::new(gz);
    ar.unpack(&pkg_dir).map_err(|e| PackageError::RegistryFailed {
        name: pkg_name.to_string(),
        registry: registry.to_string(),
        version: version.to_string(),
        detail: format!("extracting archive: {e}"),
    })?;

    pkg_dir.canonicalize().map_err(|e| PackageError::Io {
        path: pkg_dir.display().to_string(),
        detail: e.to_string(),
    })
}

fn packages_cache_dir() -> Result<PathBuf, PackageError> {
    if let Ok(dir) = std::env::var("LEX_PACKAGES_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| PackageError::Io {
            path: "~/.lex/packages".into(),
            detail: "could not determine home directory (set LEX_PACKAGES_DIR)".into(),
        })?;
    Ok(PathBuf::from(home).join(".lex").join("packages"))
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("no lex.toml found searching up from {searched_from} (needed to resolve \"{reference}\")")]
    NoManifest { reference: String, searched_from: String },

    #[error("failed to parse {path}: {detail}")]
    ManifestParse { path: String, detail: String },

    #[error("package \"{name}\" not found in {manifest}")]
    UnknownPackage { name: String, manifest: String },

    #[error("module \"{module}\" not found in package \"{pkg}\" (looked in {pkg_root}/src/ and {pkg_root}/)")]
    ModuleNotFound { pkg: String, module: String, pkg_root: String },

    #[error("git clone of {url} failed: {detail}")]
    GitFailed { url: String, detail: String },

    #[error("registry fetch of {name}@{version} from {registry} failed: {detail}")]
    RegistryFailed { name: String, registry: String, version: String, detail: String },

    #[error("I/O error at {path}: {detail}")]
    Io { path: String, detail: String },
}
