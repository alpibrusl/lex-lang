//! Store-root resolution shared by every store-backed command (#772): `--store` > `LEX_STORE` > `[store] path` in the nearest `lex.toml` > `<project>/.lex/store` > the global `$HOME/.lex/store`, noted once on stderr.

use super::*;

/// Where store-touching commands look when no `--store` is given (#772).
///
/// `LEX_STORE` wins. Otherwise the nearest `lex.toml` above the working
/// directory scopes the store to the project: its `[store] path` if
/// declared (relative to the manifest's directory), else
/// `<project>/.lex/store`. Only outside any project does this fall back
/// to the machine-wide `$HOME/.lex/store` (`.lex-store` with no HOME),
/// and then it says so once on stderr: a branch, publish, or merge that
/// lands in a store shared by every project on the machine must never
/// be silent. Until 0.10.14 the global store was the default everywhere,
/// so two unrelated projects silently shared one branch namespace.
pub(super) fn default_store_root() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let (root, scoped) = resolve_store_root(cwd.as_deref());
    if !scoped {
        note_global_store_once(&root);
    }
    root
}

/// Default location of a project's store, relative to its `lex.toml`.
pub(crate) const PROJECT_STORE_DIR: &str = ".lex/store";

/// The store root for `cwd` and whether something scopes it (`LEX_STORE`
/// or a project manifest). `false` means the machine-wide fallback.
pub(crate) fn resolve_store_root(cwd: Option<&std::path::Path>) -> (PathBuf, bool) {
    if let Ok(s) = std::env::var("LEX_STORE") {
        return (PathBuf::from(s), true);
    }
    if let Some((toml_path, toml_dir)) = cwd.and_then(lex_syntax::find_manifest) {
        let declared = lex_syntax::Manifest::load(&toml_path)
            .ok()
            .and_then(|m| m.store)
            .and_then(|st| st.path);
        let rel = declared.unwrap_or_else(|| PROJECT_STORE_DIR.to_string());
        return (toml_dir.join(rel), true);
    }
    (global_store_root(), false)
}

/// The machine-wide store: every project without a `lex.toml` shares it.
pub(crate) fn global_store_root() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".lex").join("store");
    }
    PathBuf::from(".lex-store")
}

pub(super) fn note_global_store_once(root: &std::path::Path) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static NOTED: AtomicBool = AtomicBool::new(false);
    if NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "note: no lex.toml above the working directory, so this touches the shared \
         global store at {} (every project without its own lex.toml shares it; \
         run `lex pkg init`, pass --store, or set LEX_STORE to scope it)",
        root.display()
    );
}

/// Public re-export for sibling CLI modules. `default_store_root`
/// itself stays private to keep the binary's surface tight; modules
/// that need it call this trampoline.
pub(crate) fn default_store_root_pub() -> PathBuf {
    default_store_root()
}

pub(super) fn parse_store_flag(args: &[String]) -> (PathBuf, Vec<String>, bool, bool) {
    // Returns (store_root, remaining_args, activate, dry_run).
    let mut root: Option<PathBuf> = None;
    let mut activate = false;
    let mut dry_run = false;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--store" => {
                if let Some(v) = args.get(i + 1) {
                    root = Some(PathBuf::from(v));
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--activate" => {
                activate = true;
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            _ => {
                rest.push(args[i].clone());
                i += 1;
            }
        }
    }
    (root.unwrap_or_else(default_store_root), rest, activate, dry_run)
}
