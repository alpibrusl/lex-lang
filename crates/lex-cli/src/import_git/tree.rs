//! The incremental tree: the importer's in-memory view of "the git tree of the
//! last commit processed", updated ONLY from that commit's changed paths.
//!
//! This is the scale story (#892 §1.7). A per-commit `ls-tree` + re-read of
//! every manifest file is O(head) per commit; here a commit costs O(changed
//! files): `git diff-tree` names them, a bounded oid → blob cache means an
//! unchanged (or reverted, or duplicated) blob is never re-read, and only the
//! changed `lex.toml`/`lex.lock`/`src/**` files are written into the private
//! scratch tree the loader reads. Nothing here decides whether a commit is
//! importable — [`TreeState::blocker`] reports what would stop it and the
//! caller turns that into a refusal.

use super::git::{diff_tree_changes, ls_tree_changes, RawChange};
use super::{has_package_name, ImportState, Refusal, Unsupported, MANIFEST_MAX_ENTRIES};
use anyhow::{anyhow, Context, Result};
use lex_store::files::{is_reserved_path, validate_path};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Forget the oid → blob cache when it grows past this many entries: a bound,
/// not an LRU (a miss just costs one more object read).
const CACHE_CAP: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Class {
    /// Everything but `src/**/*.lex`: goes into the files manifest.
    Manifest,
    /// `src/**/*.lex` (op-log-owned): loaded by the semantic pass.
    LexSrc,
    /// `src.lex` at the root: reserved and not importable.
    RootSrcLex,
}

#[derive(Debug, Clone)]
pub(super) struct FileState {
    pub exec: bool,
    pub size: u64,
    /// The stored blob (manifest files only).
    pub blob: Option<String>,
    pub class: Class,
}

/// What is known about a git object without re-reading it.
#[derive(Debug, Clone)]
pub(super) struct ObjMeta {
    size: u64,
    blob: Option<String>,
}

#[derive(Default)]
pub(super) struct TreeState {
    pub files: BTreeMap<String, FileState>,
    /// lowercased path → the paths that fold onto it (a collision when >1).
    lower: BTreeMap<String, BTreeSet<String>>,
    colliding: usize,
    pub manifest_count: usize,
    pub lex_src_count: usize,
    pub root_src_lex: bool,
    /// Paths (by lossy name) that make the tree unimportable while present.
    blockers: BTreeMap<String, Refusal>,
    /// Symlinks/submodules present in the tree now.
    pub unsupported_now: BTreeMap<String, &'static str>,
    pub toml_has_name: bool,
    /// The tree's `lex.lock` text (rides on every new head as its committed lock).
    pub lock_text: Option<String>,
}

impl TreeState {
    fn remove_path(&mut self, path: &str) {
        if let Some(f) = self.files.remove(path) {
            match f.class {
                Class::Manifest => self.manifest_count -= 1,
                Class::LexSrc => self.lex_src_count -= 1,
                Class::RootSrcLex => self.root_src_lex = false,
            }
            let key = path.to_lowercase();
            if let Some(set) = self.lower.get_mut(&key) {
                let was = set.len();
                set.remove(path);
                if was == 2 {
                    self.colliding -= 1;
                }
                if set.is_empty() {
                    self.lower.remove(&key);
                }
            }
        }
        self.blockers.remove(path);
        self.unsupported_now.remove(path);
        match path {
            "lex.toml" => self.toml_has_name = false,
            "lex.lock" => self.lock_text = None,
            _ => {}
        }
    }

    fn insert_file(&mut self, path: String, f: FileState) {
        match f.class {
            Class::Manifest => self.manifest_count += 1,
            Class::LexSrc => self.lex_src_count += 1,
            Class::RootSrcLex => self.root_src_lex = true,
        }
        let set = self.lower.entry(path.to_lowercase()).or_default();
        set.insert(path.clone());
        if set.len() == 2 {
            self.colliding += 1;
        }
        self.files.insert(path, f);
    }

    /// The reason (if any) the current tree cannot be imported: an unusable or
    /// oversized path, a case collision, too many manifest entries. Checked
    /// against the whole tree, so a persistent problem refuses every commit
    /// that still contains it (and stops once the path is gone).
    pub(super) fn blocker(&self) -> Option<Refusal> {
        if let Some(r) = self.blockers.values().next() {
            return Some(r.clone());
        }
        if self.colliding > 0 {
            let set = self.lower.values().find(|s| s.len() > 1).expect("colliding > 0");
            let mut it = set.iter();
            let (a, b) = (it.next().expect("2 paths"), it.next().expect("2 paths"));
            return Some(Refusal::new(
                "manifest:case_collision",
                "manifest",
                format!("paths `{a}` and `{b}` differ only in case"),
            ));
        }
        if self.manifest_count > MANIFEST_MAX_ENTRIES {
            return Some(Refusal::new(
                "manifest:limit",
                "manifest",
                format!("{} files, over the {MANIFEST_MAX_ENTRIES}-entry manifest limit", self.manifest_count),
            ));
        }
        None
    }
}

/// A path the semantic pass reads: a commit touching none of these (and
/// following a good one) is a `SetFiles`-only commit.
fn is_semantic_path(path: &str) -> bool {
    path == "lex.toml" || path == "lex.lock" || is_reserved_path(path)
}

/// A path materialized into the scratch tree the loader reads.
fn in_scratch(path: &str) -> bool {
    path == "lex.toml" || path == "lex.lock" || path.starts_with("src/")
}

/// The outcome of advancing the tree by one commit.
pub(super) struct Advance {
    /// The commit changed `lex.toml`, `lex.lock` or a `src/**/*.lex` path.
    pub semantic_touched: bool,
}

impl ImportState {
    fn scratch_file(&self, rel: &str) -> PathBuf {
        self.scratch.path().join(rel)
    }

    /// Empty the tree and the scratch dir (a fresh snapshot, or recovery after
    /// a failure part-way through an advance).
    pub(super) fn reset_tree(&mut self) {
        self.tree = TreeState::default();
        self.prev = None;
        if let Ok(rd) = std::fs::read_dir(self.scratch.path()) {
            for e in rd.flatten() {
                let p = e.path();
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
        }
    }

    /// Forget `path` everywhere, including its scratch file (and any parent
    /// directories that became empty, so a directory can turn into a file).
    fn drop_path(&mut self, path: &str) {
        self.tree.remove_path(path);
        if in_scratch(path) {
            let root = self.scratch.path().to_path_buf();
            let mut p = self.scratch_file(path);
            let _ = std::fs::remove_file(&p);
            while p.pop() && p != root {
                if std::fs::remove_dir(&p).is_err() {
                    break;
                }
            }
        }
    }

    fn note_unsupported(&mut self, path: String, kind: &'static str, sha: &str) {
        self.tree.unsupported_now.insert(path.clone(), kind);
        if !self.unsupported.iter().any(|u| u.path == path && u.kind == kind) {
            self.unsupported.push(Unsupported {
                path,
                kind,
                first_seen: sha.to_string(),
                last_seen: sha.to_string(),
            });
        }
    }

    fn obj_size(&mut self, oid: &str) -> Result<u64> {
        if let Some(m) = self.cache.get(oid) {
            return Ok(m.size);
        }
        self.info.size(oid)
    }

    /// Bring the tree (and the scratch dir) to `sha`'s state: from the previous
    /// commit's tree by `diff-tree`, or from nothing by `ls-tree` when there is
    /// no previous commit. On `Err` the tree is left half-updated: the caller
    /// must [`reset_tree`](Self::reset_tree).
    pub(super) fn advance_tree(&mut self, sha: &str) -> Result<Advance> {
        let changes = match self.prev.clone() {
            Some(prev) => diff_tree_changes(&self.repo, &prev, sha)?,
            None => {
                self.reset_tree();
                ls_tree_changes(&self.repo, sha)?
            }
        };
        let mut touched = false;
        // Deletions first, so a directory can become a file (and back).
        for c in changes.iter().filter(|c| c.mode == 0) {
            match std::str::from_utf8(&c.path) {
                Ok(p) => {
                    touched |= is_semantic_path(p);
                    self.drop_path(p);
                }
                Err(_) => {
                    self.tree.blockers.remove(String::from_utf8_lossy(&c.path).as_ref());
                }
            }
        }
        for c in changes.iter().filter(|c| c.mode != 0) {
            self.upsert(c, sha, &mut touched)?;
        }
        // Everything unsupported that is still in the tree was seen at `sha`.
        let now: Vec<(String, &'static str)> =
            self.tree.unsupported_now.iter().map(|(p, k)| (p.clone(), *k)).collect();
        for (p, k) in now {
            if let Some(u) = self.unsupported.iter_mut().find(|u| u.path == p && u.kind == k) {
                u.last_seen = sha.to_string();
            }
        }
        self.prev = Some(sha.to_string());
        Ok(Advance { semantic_touched: touched })
    }

    fn upsert(&mut self, c: &RawChange, sha: &str, touched: &mut bool) -> Result<()> {
        let Ok(path) = String::from_utf8(c.path.clone()) else {
            let lossy = String::from_utf8_lossy(&c.path).to_string();
            self.tree.blockers.insert(
                lossy.clone(),
                Refusal::new("manifest:path", "tree", format!("a path is not valid UTF-8: {lossy}")),
            );
            return Ok(());
        };
        *touched |= is_semantic_path(&path);
        self.drop_path(&path);

        // Symlinks and submodules are not representable (files-v1): skip + list.
        let kind = match c.mode & 0o170000 {
            0o120000 => Some("symlink"),
            0o160000 => Some("submodule"),
            _ => None,
        };
        if let Some(kind) = kind {
            self.note_unsupported(path, kind, sha);
            return Ok(());
        }
        // The store's own directory / git internals are never content.
        let first = path.split('/').next().unwrap_or("");
        if path.split('/').any(|p| p.eq_ignore_ascii_case(".git")) || first.eq_ignore_ascii_case(".lex") {
            return Ok(());
        }
        if path.split('/').any(|p| p.is_empty() || p == "." || p == "..") || path.contains('\0') {
            self.tree
                .blockers
                .insert(path.clone(), Refusal::new("manifest:path", "tree", format!("unusable path `{path}`")));
            return Ok(());
        }
        let size = self.obj_size(&c.oid)?;
        if size > self.max_file_bytes {
            self.tree.blockers.insert(
                path.clone(),
                Refusal::new(
                    "manifest:limit",
                    "manifest",
                    format!("`{path}` is {size} bytes, over the {}-byte per-file limit", self.max_file_bytes),
                ),
            );
            return Ok(());
        }
        let class = if path == "src.lex" {
            Class::RootSrcLex
        } else if is_reserved_path(&path) {
            Class::LexSrc
        } else {
            Class::Manifest
        };
        if class == Class::Manifest {
            if let Err(err) = validate_path(&path) {
                self.tree.blockers.insert(path, Refusal::new("manifest:path", "manifest", err.to_string()));
                return Ok(());
            }
        }

        // Bytes: only for what needs them, and only when the cache cannot say.
        let scratch = class != Class::RootSrcLex && in_scratch(&path);
        let cached_blob = if class == Class::Manifest {
            self.cache.get(&c.oid).and_then(|m| m.blob.clone())
        } else {
            None
        };
        let mut blob = cached_blob;
        if scratch || (class == Class::Manifest && blob.is_none()) {
            let bytes = self.cat.read(&c.oid)?;
            if class == Class::Manifest && blob.is_none() {
                blob = Some(self.store.put_blob_bytes(&bytes).map_err(|e| anyhow!("storing blob for `{path}`: {e}"))?);
            }
            if scratch {
                let dest = self.scratch_file(&path);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
            }
            match path.as_str() {
                "lex.toml" => self.tree.toml_has_name = has_package_name(&bytes),
                "lex.lock" => self.tree.lock_text = String::from_utf8(bytes).ok(),
                _ => {}
            }
        } else if class == Class::Manifest {
            self.stats.cache_hits += 1;
        }
        let known = self.cache.get(&c.oid).and_then(|m| m.blob.clone());
        if self.cache.len() >= CACHE_CAP {
            self.cache.clear();
        }
        self.cache.insert(c.oid.clone(), ObjMeta { size, blob: blob.clone().or(known) });
        self.tree.insert_file(
            path,
            FileState { exec: c.mode & 0o111 != 0, size, blob, class },
        );
        Ok(())
    }
}
