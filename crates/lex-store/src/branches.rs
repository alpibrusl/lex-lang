//! Snapshot branches (tier-1 of agent-native version control).
//!
//! A branch is a named map `SigId → StageId`: which implementation of
//! each signature is "live" on that branch. Branches sit alongside
//! the existing lifecycle (`Active` / `Deprecated` / ...) — `main`
//! is the default branch, and operations that don't explicitly name
//! a branch operate on `main`. The legacy lifecycle remains the
//! source of truth for `main`'s head (we materialize it on demand);
//! other branches store their heads explicitly under
//! `<root>/branches/<name>.json`.
//!
//! What's deferred (tracked for follow-up rounds):
//!
//! - **Commit history.** A branch is a current-state snapshot, not
//!   a sequence of commits. `lex log` doesn't exist yet.
//! - **Distributed sync.** No push/pull between stores.
//! - **Identity / authorship.** Stages don't carry author metadata.
//! - **Body-level merge.** The merge operation pairs SigIds and
//!   reports conflicts when both sides changed; it doesn't yet do
//!   intra-stage AST patching (that's `lex ast-merge`'s territory).
//!
//! What ships:
//!
//! - `Store::current_branch` / `set_current_branch`
//! - `list_branches` / `get_branch` / `create_branch` / `delete_branch`
//! - `branch_head` reads the live head map; for `main` it walks
//!   lifecycle.json so existing stores work without migration.
//! - `set_branch_head_entry` updates a single (SigId, StageId) pair.
//! - `merge` performs a top-level merge of two branches against a
//!   common ancestor (computed from `parent` chains); conflicts come
//!   back as structured JSON.
//!
//! Persistence layout adds:
//!
//! ```text
//! <root>/
//! ├── branches/<name>.json   # { name, parent, head: {SigId: StageId}, created_at }
//! └── current_branch         # plain text: branch name
//! ```

use crate::store::{Store, StoreError};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Default branch name when no `current_branch` file exists.
pub const DEFAULT_BRANCH: &str = "main";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Branch {
    pub name: String,
    /// Parent branch this one was forked from. `None` means
    /// "no parent" — the root of the branch graph (typically `main`
    /// itself, or a branch created in a fresh store).
    pub parent: Option<String>,
    /// SigId → StageId map for stages that are live on this branch.
    pub head: BTreeMap<String, String>,
    /// Snapshot of the parent branch's head at fork time. Used as
    /// the immutable common ancestor when this branch is merged.
    /// Default `None` means "no fork-base recorded" — back-compat
    /// for older branch files; merge falls through to current parent
    /// head, which can produce false-clean results if the parent
    /// has since moved. Branches created via `create_branch` always
    /// have this populated.
    #[serde(default)]
    pub fork_base: Option<BTreeMap<String, String>>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeReport {
    pub summary: MergeSummary,
    pub merged: Vec<MergeEntry>,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MergeSummary {
    pub total_sigs: usize,
    pub clean: usize,
    pub conflicts: usize,
    pub base: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeEntry {
    pub sig_id: String,
    pub stage_id: String,
    /// "base" / "src" / "dst" / "both" / "added-src" / "added-dst" /
    /// "added-both".
    pub from: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeConflict {
    pub sig_id: String,
    /// "modify-modify" / "modify-delete" / "delete-modify" / "add-add".
    pub kind: &'static str,
    pub base:   Option<String>,
    pub src:    Option<String>,
    pub dst:    Option<String>,
}

impl Store {
    fn branches_dir(&self) -> PathBuf { self.root().join("branches") }
    fn branch_path(&self, name: &str) -> PathBuf {
        self.branches_dir().join(format!("{name}.json"))
    }
    fn current_branch_path(&self) -> PathBuf {
        self.root().join("current_branch")
    }

    pub fn current_branch(&self) -> String {
        match fs::read_to_string(self.current_branch_path()) {
            Ok(s) => s.trim().to_string(),
            Err(_) => DEFAULT_BRANCH.to_string(),
        }
    }

    pub fn set_current_branch(&self, name: &str) -> Result<(), StoreError> {
        // Lazy materialization: looking up a non-existent branch by
        // making it current is a useful error signal.
        if name != DEFAULT_BRANCH && self.get_branch(name)?.is_none() {
            return Err(StoreError::UnknownBranch(name.into()));
        }
        fs::write(self.current_branch_path(), name)?;
        Ok(())
    }

    pub fn list_branches(&self) -> Result<Vec<String>, StoreError> {
        let mut out: Vec<String> = vec![DEFAULT_BRANCH.into()];
        let dir = self.branches_dir();
        if !dir.exists() { return Ok(out); }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    if name != DEFAULT_BRANCH { out.push(name.to_string()); }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn get_branch(&self, name: &str) -> Result<Option<Branch>, StoreError> {
        let path = self.branch_path(name);
        if !path.exists() { return Ok(None); }
        let raw = fs::read_to_string(&path)?;
        let b: Branch = serde_json::from_str(&raw)?;
        Ok(Some(b))
    }

    /// Read a branch's head map. For `main`, materialize from the
    /// legacy lifecycle.json files if no explicit branch file exists.
    pub fn branch_head(&self, name: &str) -> Result<BTreeMap<String, String>, StoreError> {
        if let Some(b) = self.get_branch(name)? {
            return Ok(b.head);
        }
        if name == DEFAULT_BRANCH {
            // Walk every SigId; for each, look up the current Active.
            let mut head = BTreeMap::new();
            for sig in self.list_sigs()? {
                if let Some(stage) = self.resolve_sig(&sig)? {
                    head.insert(sig, stage);
                }
            }
            return Ok(head);
        }
        Err(StoreError::UnknownBranch(name.into()))
    }

    /// Snapshot the source branch's head into a new named branch.
    /// Errors if the branch already exists. Sets `parent` to `from`.
    pub fn create_branch(&self, name: &str, from: &str) -> Result<(), StoreError> {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(StoreError::InvalidTransition(
                format!("branch name `{name}` rejected (empty or path-like)")));
        }
        if self.branch_path(name).exists() {
            return Err(StoreError::InvalidTransition(
                format!("branch `{name}` already exists")));
        }
        let head = self.branch_head(from)?;
        fs::create_dir_all(self.branches_dir())?;
        let b = Branch {
            name: name.into(),
            parent: Some(from.into()),
            fork_base: Some(head.clone()),
            head,
            created_at: now(),
        };
        fs::write(self.branch_path(name), serde_json::to_string_pretty(&b)?)?;
        Ok(())
    }

    pub fn delete_branch(&self, name: &str) -> Result<(), StoreError> {
        if name == DEFAULT_BRANCH {
            return Err(StoreError::InvalidTransition(
                "cannot delete the default branch".into()));
        }
        if self.current_branch() == name {
            return Err(StoreError::InvalidTransition(format!(
                "cannot delete `{name}`; check out another branch first")));
        }
        let path = self.branch_path(name);
        if !path.exists() {
            return Err(StoreError::UnknownBranch(name.into()));
        }
        fs::remove_file(path)?;
        Ok(())
    }

    /// Update one (SigId → StageId) entry on a named branch. For
    /// `main`, this materializes the lazy head into an explicit
    /// branch file before overwriting the entry, so subsequent
    /// reads see the override.
    pub fn set_branch_head_entry(
        &self,
        name: &str,
        sig: &str,
        stage: &str,
    ) -> Result<(), StoreError> {
        let mut b = match self.get_branch(name)? {
            Some(b) => b,
            None if name == DEFAULT_BRANCH => Branch {
                name: DEFAULT_BRANCH.into(),
                parent: None,
                head: self.branch_head(DEFAULT_BRANCH)?,
                fork_base: None,
                created_at: now(),
            },
            None => return Err(StoreError::UnknownBranch(name.into())),
        };
        b.head.insert(sig.to_string(), stage.to_string());
        fs::create_dir_all(self.branches_dir())?;
        fs::write(self.branch_path(name), serde_json::to_string_pretty(&b)?)?;
        Ok(())
    }

    /// Three-way merge of two branches; the common ancestor is
    /// computed from the `parent` chain. If no common ancestor is
    /// found, fall back to two-way (every divergence is a conflict).
    /// Result is *not* committed automatically — callers inspect the
    /// MergeReport, optionally resolve, and then write the merged
    /// head themselves via `commit_merge`.
    pub fn merge(&self, src: &str, dst: &str) -> Result<MergeReport, StoreError> {
        let src_head = self.branch_head(src)?;
        let dst_head = self.branch_head(dst)?;
        let (base_head, base_name) = self.compute_merge_base(src, dst)?;

        let mut report = MergeReport {
            summary: MergeSummary {
                base: base_name.clone(),
                ..Default::default()
            },
            merged: Vec::new(),
            conflicts: Vec::new(),
        };
        let names: std::collections::BTreeSet<&String> = base_head.keys()
            .chain(src_head.keys()).chain(dst_head.keys()).collect();
        for sig in &names {
            let b = base_head.get(*sig);
            let s = src_head.get(*sig);
            let d = dst_head.get(*sig);
            match (b, s, d) {
                (Some(_), Some(s_id), Some(d_id)) => {
                    if s_id == d_id {
                        // Same on both sides — clean.
                        let from = if Some(s_id) == b { "base" } else { "both" };
                        report.merged.push(MergeEntry {
                            sig_id: (*sig).clone(), stage_id: s_id.clone(), from,
                        });
                    } else if Some(s_id) == b {
                        // Only dst diverged.
                        report.merged.push(MergeEntry {
                            sig_id: (*sig).clone(), stage_id: d_id.clone(), from: "dst",
                        });
                    } else if Some(d_id) == b {
                        // Only src diverged.
                        report.merged.push(MergeEntry {
                            sig_id: (*sig).clone(), stage_id: s_id.clone(), from: "src",
                        });
                    } else {
                        report.conflicts.push(MergeConflict {
                            sig_id: (*sig).clone(),
                            kind: "modify-modify",
                            base: b.cloned(), src: Some(s_id.clone()), dst: Some(d_id.clone()),
                        });
                    }
                }
                (Some(b_id), Some(s_id), None) => {
                    if s_id == b_id {
                        // dst deleted, src unchanged → take dst's delete.
                    } else {
                        report.conflicts.push(MergeConflict {
                            sig_id: (*sig).clone(),
                            kind: "modify-delete",
                            base: Some(b_id.clone()), src: Some(s_id.clone()), dst: None,
                        });
                    }
                }
                (Some(b_id), None, Some(d_id)) => {
                    if d_id == b_id {
                        // src deleted, dst unchanged → take src's delete.
                    } else {
                        report.conflicts.push(MergeConflict {
                            sig_id: (*sig).clone(),
                            kind: "delete-modify",
                            base: Some(b_id.clone()), src: None, dst: Some(d_id.clone()),
                        });
                    }
                }
                (None, Some(s_id), Some(d_id)) => {
                    if s_id == d_id {
                        report.merged.push(MergeEntry {
                            sig_id: (*sig).clone(), stage_id: s_id.clone(), from: "added-both",
                        });
                    } else {
                        report.conflicts.push(MergeConflict {
                            sig_id: (*sig).clone(),
                            kind: "add-add",
                            base: None, src: Some(s_id.clone()), dst: Some(d_id.clone()),
                        });
                    }
                }
                (None, Some(s_id), None) => report.merged.push(MergeEntry {
                    sig_id: (*sig).clone(), stage_id: s_id.clone(), from: "added-src",
                }),
                (None, None, Some(d_id)) => report.merged.push(MergeEntry {
                    sig_id: (*sig).clone(), stage_id: d_id.clone(), from: "added-dst",
                }),
                (Some(_), None, None) => {} // both deleted — clean removal
                (None, None, None) => unreachable!(),
            }
        }
        report.summary.clean = report.merged.len();
        report.summary.conflicts = report.conflicts.len();
        report.summary.total_sigs = report.merged.len() + report.conflicts.len();
        Ok(report)
    }

    /// Apply a clean merge to `dst`. Refuses if any conflicts remain.
    pub fn commit_merge(&self, dst: &str, report: &MergeReport) -> Result<(), StoreError> {
        if !report.conflicts.is_empty() {
            return Err(StoreError::InvalidTransition(format!(
                "{} conflicts; resolve before committing", report.conflicts.len())));
        }
        let mut b = match self.get_branch(dst)? {
            Some(b) => b,
            None if dst == DEFAULT_BRANCH => Branch {
                name: DEFAULT_BRANCH.into(),
                parent: None,
                head: self.branch_head(DEFAULT_BRANCH)?,
                fork_base: None,
                created_at: now(),
            },
            None => return Err(StoreError::UnknownBranch(dst.into())),
        };
        // Replace head from report.merged.
        let mut head = BTreeMap::new();
        for m in &report.merged {
            head.insert(m.sig_id.clone(), m.stage_id.clone());
        }
        b.head = head;
        fs::create_dir_all(self.branches_dir())?;
        fs::write(self.branch_path(dst), serde_json::to_string_pretty(&b)?)?;
        Ok(())
    }

    /// Pick the head map to use as the three-way merge base.
    ///
    /// Branches forked via `create_branch` carry `fork_base`: a
    /// snapshot of the parent's head at fork time. That snapshot is
    /// the correct ancestor — re-resolving the parent's current head
    /// would falsely treat post-fork changes on the parent as
    /// "always-there", flipping genuine modify-modify conflicts into
    /// silent clean merges.
    fn compute_merge_base(
        &self,
        src: &str,
        dst: &str,
    ) -> Result<(BTreeMap<String, String>, Option<String>), StoreError> {
        let src_b = self.get_branch(src)?;
        let dst_b = self.get_branch(dst)?;

        // src forked off (a chain ending at) dst → src's snapshot wins.
        if let Some(b) = &src_b {
            let chain = self.parent_chain(src)?;
            if chain.iter().any(|n| n == dst) {
                if let Some(fb) = &b.fork_base {
                    return Ok((fb.clone(), Some(format!("{src}@fork"))));
                }
            }
        }
        // dst forked off src.
        if let Some(b) = &dst_b {
            let chain = self.parent_chain(dst)?;
            if chain.iter().any(|n| n == src) {
                if let Some(fb) = &b.fork_base {
                    return Ok((fb.clone(), Some(format!("{dst}@fork"))));
                }
            }
        }
        // Siblings sharing a parent: prefer src's snapshot.
        if let (Some(s), Some(d)) = (&src_b, &dst_b) {
            if s.parent.is_some() && s.parent == d.parent {
                if let Some(fb) = &s.fork_base {
                    return Ok((fb.clone(), Some(format!("{src}@fork"))));
                }
                if let Some(fb) = &d.fork_base {
                    return Ok((fb.clone(), Some(format!("{dst}@fork"))));
                }
            }
        }
        // Last resort: legacy parent-chain ancestor's *current* head.
        // Used for branch files predating the `fork_base` field.
        if let Some(name) = self.find_common_ancestor(src, dst)? {
            let head = self.branch_head(&name)?;
            return Ok((head, Some(name)));
        }
        Ok((BTreeMap::new(), None))
    }

    /// Walk parent chains to find a common ancestor. Returns the
    /// name of the closest one if any exists; `None` if the branches
    /// have no shared ancestry.
    fn find_common_ancestor(&self, a: &str, b: &str) -> Result<Option<String>, StoreError> {
        let chain_a = self.parent_chain(a)?;
        let chain_b = self.parent_chain(b)?;
        let set_b: std::collections::BTreeSet<&String> = chain_b.iter().collect();
        for name in &chain_a {
            if set_b.contains(name) { return Ok(Some(name.clone())); }
        }
        Ok(None)
    }

    fn parent_chain(&self, start: &str) -> Result<Vec<String>, StoreError> {
        let mut out = vec![start.to_string()];
        let mut cur = start.to_string();
        let mut seen = std::collections::BTreeSet::new();
        seen.insert(cur.clone());
        while let Some(b) = self.get_branch(&cur)? {
            match b.parent {
                Some(p) if !seen.contains(&p) => {
                    seen.insert(p.clone());
                    out.push(p.clone());
                    cur = p;
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

// Provide the IndexMap-iter helper used in some downstream callers.
#[allow(dead_code)]
fn _ensure_indexmap() -> IndexMap<String, String> { IndexMap::new() }

fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
