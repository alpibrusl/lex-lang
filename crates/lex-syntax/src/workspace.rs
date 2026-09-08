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
//! # pin to a tag:
//! lex-schema = { git = "https://github.com/alpibrusl/lex-schema", tag = "v1.2.0" }
//! # pin to a commit:
//! lex-schema = { git = "https://github.com/alpibrusl/lex-schema", rev = "abc1234" }
//! # track a branch:
//! lex-schema = { git = "https://github.com/alpibrusl/lex-schema", branch = "stable" }
//! # or from a registry:
//! lex-schema = { registry = "https://lexhub.alpibru.com", version = "0.9.2" }
//! ```
//!
//! At most one of `tag`, `rev`, `branch` may be set; omitting all three
//! clones the default branch at HEAD (not reproducible — pin for releases).
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
//! 4. For `git =`: clones the repo into `~/.lex/packages/lex-schema-<ref>/`
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
    /// `[store]` — where store-touching commands (`lex branch`,
    /// `publish`, `merge`, `op`, `store`, ...) keep this project's
    /// stages, branches, and traces when no `--store` is given (#772).
    #[serde(default)]
    pub store: Option<StoreSection>,
}

/// `[store]` table of `lex.toml`.
#[derive(Debug, Default, Deserialize)]
pub struct StoreSection {
    /// Store root, relative to the manifest's directory (an absolute
    /// path is used as is). Defaults to `.lex/store` when absent.
    #[serde(default)]
    pub path: Option<String>,
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
    /// Minimum toolchain this package needs, as `lex = "0.10.15"`.
    ///
    /// Packages across the org have declared this for a long time; until
    /// #803 nothing read it, because serde silently drops unknown fields.
    /// A dependency that had moved onto a newer stdlib installed without
    /// complaint into a project pinned older, and the mismatch surfaced
    /// as `unknown_field` errors naming a function nobody in the
    /// consuming repo had written.
    #[serde(default)]
    pub lex: Option<String>,
}

/// Parse a `MAJOR.MINOR.PATCH` version into comparable parts.
///
/// Deliberately not a semver crate: these are toolchain pins written by
/// hand in `lex.toml`, always three numeric components, and a
/// dependency that spells its floor some other way should be skipped
/// rather than guessed at.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.trim().trim_start_matches('v').split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Does `running` satisfy a declared `floor`?
///
/// `None` when either side is unparseable — the caller should say so
/// rather than treat "cannot tell" as either satisfied or violated.
pub fn satisfies_floor(running: &str, floor: &str) -> Option<bool> {
    Some(parse_version(running)? >= parse_version(floor)?)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Dependency {
    Path     { path: String },
    Git      {
        git:    String,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        tag:    Option<String>,
        #[serde(default)]
        rev:    Option<String>,
    },
    Registry { registry: String, version: String },
}

impl Dependency {
    /// Return an error if more than one of branch/tag/rev is set.
    pub fn validate(&self) -> Result<(), String> {
        if let Dependency::Git { branch, tag, rev, .. } = self {
            let count = [branch, tag, rev].iter().filter(|o| o.is_some()).count();
            if count > 1 {
                return Err("at most one of `branch`, `tag`, `rev` may be set on a git dependency".into());
            }
        }
        Ok(())
    }
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
        Dependency::Git { git, branch, tag, rev } => {
            dep.validate().map_err(|e| PackageError::ManifestParse {
                path: toml_path.display().to_string(),
                detail: e,
            })?;
            let git_ref = GitRef::from(branch.as_deref(), tag.as_deref(), rev.as_deref());
            git_ensure_cached(pkg_name, git, &git_ref)?
        }
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

/// Parsed ref from a git dependency declaration.
#[derive(Debug)]
enum GitRef<'a> {
    Branch(&'a str),
    Tag(&'a str),
    Rev(&'a str),
    DefaultBranch,
}

impl<'a> GitRef<'a> {
    fn from(branch: Option<&'a str>, tag: Option<&'a str>, rev: Option<&'a str>) -> Self {
        if let Some(b) = branch { return GitRef::Branch(b); }
        if let Some(t) = tag    { return GitRef::Tag(t); }
        if let Some(r) = rev    { return GitRef::Rev(r); }
        GitRef::DefaultBranch
    }

    /// Slug appended to the cache directory name to prevent collisions between
    /// different refs of the same repo.
    fn cache_slug(&self) -> String {
        match self {
            GitRef::Branch(b)    => format!("@branch-{}", sanitize_ref(b)),
            GitRef::Tag(t)       => format!("@tag-{}", sanitize_ref(t)),
            GitRef::Rev(r)       => format!("@rev-{}", &r[..r.len().min(12)]),
            GitRef::DefaultBranch => String::new(),
        }
    }
}

/// Replace characters that are not safe in directory names.
fn sanitize_ref(r: &str) -> String {
    r.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

/// Return the local cache directory for `pkg_name`, cloning from `url`
/// at the given ref if it isn't there yet.
///
/// Cache root: `$LEX_PACKAGES_DIR` if set, otherwise `~/.lex/packages/`.
/// Cache key:  `{pkg_name}{ref_slug}` so different tags/revs don't collide.
fn git_ensure_cached(pkg_name: &str, url: &str, git_ref: &GitRef<'_>) -> Result<PathBuf, PackageError> {
    let cache_root = packages_cache_dir()?;
    let dir_name = format!("{}{}", pkg_name, git_ref.cache_slug());
    let pkg_dir = cache_root.join(&dir_name);
    if pkg_dir.exists() {
        return Ok(pkg_dir);
    }
    std::fs::create_dir_all(&cache_root).map_err(|e| PackageError::Io {
        path: cache_root.display().to_string(),
        detail: e.to_string(),
    })?;

    let dest = pkg_dir.to_str().unwrap_or(&dir_name);

    let status = match git_ref {
        GitRef::Rev(rev) => {
            // Shallow clone is not possible for arbitrary commits; do a full
            // clone then check out the specific revision.
            let s = run_git(&["clone", "--quiet", url, dest], url)?;
            if s {
                run_git(&["-C", dest, "checkout", "--quiet", rev], url)?;
                true
            } else {
                false
            }
        }
        GitRef::Tag(tag) => run_git(&["clone", "--quiet", "--depth=1", "--branch", tag, url, dest], url)?,
        GitRef::Branch(branch) => run_git(&["clone", "--quiet", "--depth=1", "--branch", branch, url, dest], url)?,
        GitRef::DefaultBranch  => run_git(&["clone", "--quiet", "--depth=1", url, dest], url)?,
    };

    if !status {
        // Clean up partial clone so a retry doesn't hit the cache check above.
        let _ = std::fs::remove_dir_all(&pkg_dir);
        return Err(PackageError::GitFailed {
            url: url.to_string(),
            detail: "`git` exited with non-zero status".into(),
        });
    }

    pkg_dir.canonicalize().map_err(|e| PackageError::Io {
        path: pkg_dir.display().to_string(),
        detail: e.to_string(),
    })
}

/// Run a git command and return `Ok(true)` on success, `Ok(false)` on non-zero
/// exit, or `Err` if git could not be spawned.
fn run_git(args: &[&str], url: &str) -> Result<bool, PackageError> {
    let status = std::process::Command::new("git")
        .args(args)
        .status()
        .map_err(|e| PackageError::GitFailed {
            url: url.to_string(),
            detail: format!("could not run `git`: {e}"),
        })?;
    Ok(status.success())
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

/// The flat package cache root used by the resolver — `$LEX_PACKAGES_DIR`
/// if set, otherwise `~/.lex/packages/`. Returns `None` only when neither is
/// available. Exposed so tooling can recognise dependencies whose path
/// resolves *into* this cache (and are therefore aliases of an
/// already-installed package rather than independent sources).
pub fn packages_cache_root() -> Option<PathBuf> {
    packages_cache_dir().ok()
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

#[cfg(test)]
mod floor_tests {
    use super::{parse_version, satisfies_floor};

    #[test]
    fn parses_plain_and_v_prefixed() {
        assert_eq!(parse_version("0.10.18"), Some((0, 10, 18)));
        assert_eq!(parse_version("v0.10.18"), Some((0, 10, 18)));
        assert_eq!(parse_version("  1.2.3 "), Some((1, 2, 3)));
    }

    #[test]
    fn refuses_what_it_cannot_read_rather_than_guessing() {
        // A floor it cannot parse must be reported as unknown, never
        // silently treated as met (which would defeat the check) or as
        // violated (which would fail working installs).
        for bad in ["nightly", "0.10", "0.10.18.1", "", "0.x.1", "1.2.3-rc1"] {
            assert_eq!(parse_version(bad), None, "should not parse: {bad}");
            assert_eq!(satisfies_floor("0.10.18", bad), None);
            assert_eq!(satisfies_floor(bad, "0.10.18"), None);
        }
    }

    #[test]
    fn compares_by_component_not_lexically() {
        // The case that motivated #803: 0.10.11 < 0.10.15, though a
        // string comparison says otherwise ("0.10.11" > "0.10.15" is
        // false, but "0.10.9" > "0.10.15" is true lexically).
        assert_eq!(satisfies_floor("0.10.11", "0.10.15"), Some(false));
        assert_eq!(satisfies_floor("0.10.18", "0.10.15"), Some(true));
        assert_eq!(satisfies_floor("0.10.9", "0.10.15"), Some(false));
        assert_eq!(satisfies_floor("0.9.20", "0.10.0"), Some(false));
    }

    #[test]
    fn an_exactly_met_floor_is_met() {
        assert_eq!(satisfies_floor("0.10.15", "0.10.15"), Some(true));
    }

    #[test]
    fn the_field_is_read_from_a_manifest() {
        // Before #803 this field existed in every lex.toml in the org and
        // was dropped on the floor by serde, so assert it survives parsing.
        let m: super::Manifest = toml::from_str(
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\nlex = \"0.10.15\"\n",
        )
        .expect("parse");
        assert_eq!(m.package.unwrap().lex.as_deref(), Some("0.10.15"));
    }

    #[test]
    fn a_manifest_without_the_field_still_parses() {
        let m: super::Manifest =
            toml::from_str("[package]\nname = \"p\"\nversion = \"0.1.0\"\n").expect("parse");
        assert_eq!(m.package.unwrap().lex, None);
    }
}
