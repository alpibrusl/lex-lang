//! Branches: each branch is identified by a name and a head OpId.
//! The SigId → StageId map every consumer reads is computed by
//! replaying the op log from the head back. No materialized cache.
//!
//! `lifecycle.json` (Draft/Active/Deprecated/Tombstone per stage)
//! survives as orthogonal stage-status metadata; it no longer drives
//! branch resolution.

use crate::store::{Store, StoreError};
use lex_vcs::{OpId, OpLog, StageTransition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_BRANCH: &str = "main";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Branch {
    pub name: String,
    pub parent: Option<String>,
    /// Op DAG head. `None` means the branch has never had an op
    /// applied (empty branch).
    #[serde(default)]
    pub head_op: Option<OpId>,
    /// Append-only journal of merges committed *into* this branch.
    #[serde(default)]
    pub merges: Vec<MergeRecord>,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MergeRecord {
    pub src: String,
    pub at: u64,
    pub merged: usize,
    pub conflicts: usize,
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
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub dst: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeEntry {
    pub sig_id: String,
    pub stage_id: String,
    pub from: &'static str, // "src" | "dst" | "both"
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeConflict {
    pub sig_id: String,
    pub kind: &'static str,
    pub base: Option<String>,
    pub src: Option<String>,
    pub dst: Option<String>,
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

    /// Computed view: walk the op log from the branch head and
    /// replay each transition into a SigId → StageId map.
    ///
    /// PERF: O(N) per call where N is the number of ops on this
    /// branch's history. Each call: re-opens the op log (a `mkdir
    /// -p ops/` syscall), BFS-walks the full ancestor set, allocates
    /// a `BTreeSet<OpId>` + `Vec<OperationRecord>` + `BTreeMap`,
    /// reverses, then linearly replays. No memoization. Tier-1 size
    /// (a few hundred ops per branch) makes this acceptable; if
    /// hotter consumers land (e.g. an HTTP-served `branch_head`),
    /// memoize per-(branch_name, head_op) — the head_op tail of the
    /// cache key is a content-addressed hash, so cache invalidation
    /// is free.
    pub fn branch_head(&self, name: &str) -> Result<BTreeMap<String, String>, StoreError> {
        let b = match self.get_branch(name)? {
            Some(b) => b,
            None if name == DEFAULT_BRANCH => return Ok(BTreeMap::new()),
            None => return Err(StoreError::UnknownBranch(name.into())),
        };
        let Some(head) = b.head_op else { return Ok(BTreeMap::new()); };
        let log = OpLog::open(self.root())?;
        let mut map = BTreeMap::new();
        for rec in log.walk_forward(&head, None)? {
            apply_transition(&mut map, &rec.produces);
        }
        Ok(map)
    }

    pub fn branch_log(&self, name: &str) -> Result<Vec<MergeRecord>, StoreError> {
        match self.get_branch(name)? {
            Some(b) => Ok(b.merges),
            None if name == DEFAULT_BRANCH => Ok(Vec::new()),
            None => Err(StoreError::UnknownBranch(name.into())),
        }
    }

    /// Snapshot the source branch's head_op into a new named branch.
    pub fn create_branch(&self, name: &str, from: &str) -> Result<(), StoreError> {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(StoreError::InvalidTransition(
                format!("branch name `{name}` rejected (empty or path-like)")));
        }
        if self.branch_path(name).exists() {
            return Err(StoreError::InvalidTransition(
                format!("branch `{name}` already exists")));
        }
        let head_op = self.get_branch(from)?.and_then(|b| b.head_op);
        fs::create_dir_all(self.branches_dir())?;
        let b = Branch {
            name: name.into(),
            parent: Some(from.into()),
            head_op,
            merges: Vec::new(),
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

    /// Atomically set a branch's `head_op`. Used by `apply_operation`
    /// after a successful op apply. Materializes `main`'s branch file
    /// on first call (creates `branches/main.json`).
    ///
    /// Crash safety: the tempfile's data is fsync'd before rename
    /// (see `write_branch_atomic`), so a successful return implies a
    /// durable branch file at the final path. The containing directory
    /// is not fsync'd; on a crash between rename and the directory's
    /// metadata flush, the rename can be lost — the prior head (or
    /// missing branch file for a fresh `main`) survives. The op record
    /// itself is content-addressed and is independently durable in the
    /// op log.
    ///
    /// Concurrency: single-writer per store. Two writers calling this
    /// for the same branch race on read-modify-write of the JSON file
    /// (each reads, mutates `head_op`, renames its tempfile in). Last
    /// writer wins; the loser's head update is silently dropped, even
    /// though both their op records survive in the op log. Tier-1
    /// merge / `lex publish` callers run sequentially; multi-writer
    /// safety (file locking) is on the table once `lex serve` becomes
    /// a real concurrent producer (#130 territory).
    pub(crate) fn set_branch_head_op(
        &self,
        name: &str,
        head_op: OpId,
    ) -> Result<(), StoreError> {
        let mut b = match self.get_branch(name)? {
            Some(b) => b,
            None if name == DEFAULT_BRANCH => Branch {
                name: DEFAULT_BRANCH.into(),
                parent: None,
                head_op: None,
                merges: Vec::new(),
                created_at: now(),
            },
            None => return Err(StoreError::UnknownBranch(name.into())),
        };
        b.head_op = Some(head_op);
        fs::create_dir_all(self.branches_dir())?;
        write_branch_atomic(&self.branch_path(name), &b)?;
        Ok(())
    }
}

/// Apply a single `StageTransition` to a sig-stage map. Used by
/// `branch_head` to replay an op log.
fn apply_transition(map: &mut BTreeMap<String, String>, t: &StageTransition) {
    match t {
        StageTransition::Create { sig_id, stage_id }
        | StageTransition::Replace { sig_id, to: stage_id, .. } => {
            map.insert(sig_id.clone(), stage_id.clone());
        }
        StageTransition::Remove { sig_id, .. } => {
            map.remove(sig_id);
        }
        StageTransition::Rename { from, to, body_stage_id } => {
            map.remove(from);
            map.insert(to.clone(), body_stage_id.clone());
        }
        StageTransition::ImportOnly => {}
        StageTransition::Merge { entries } => {
            for (sig, stage) in entries {
                match stage {
                    Some(s) => { map.insert(sig.clone(), s.clone()); }
                    None    => { map.remove(sig); }
                }
            }
        }
    }
}

fn write_branch_atomic(path: &std::path::Path, b: &Branch) -> Result<(), StoreError> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(b)?;
    let tmp = path.with_extension("json.tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Store {
    pub fn merge(&self, src: &str, dst: &str) -> Result<MergeReport, StoreError> {
        let log = OpLog::open(self.root())?;
        let src_head = self.get_branch(src)?.and_then(|b| b.head_op);
        let dst_head = match self.get_branch(dst)? {
            Some(b) => b.head_op,
            None if dst == DEFAULT_BRANCH => None,
            None => return Err(StoreError::UnknownBranch(dst.into())),
        };
        let out = lex_vcs::merge(&log, src_head.as_ref(), dst_head.as_ref())?;

        let mut report = MergeReport {
            summary: MergeSummary {
                base: out.lca.clone(),
                src: src.into(),
                dst: dst.into(),
                ..Default::default()
            },
            merged: Vec::new(),
            conflicts: Vec::new(),
        };
        for o in out.outcomes {
            match o {
                lex_vcs::MergeOutcome::Both { sig_id, stage_id } => {
                    if let Some(stage_id) = stage_id {
                        report.merged.push(MergeEntry { sig_id, stage_id, from: "both" });
                    }
                }
                lex_vcs::MergeOutcome::Src { sig_id, stage_id } => {
                    if let Some(stage_id) = stage_id {
                        report.merged.push(MergeEntry { sig_id, stage_id, from: "src" });
                    }
                }
                lex_vcs::MergeOutcome::Dst { sig_id, stage_id } => {
                    if let Some(stage_id) = stage_id {
                        report.merged.push(MergeEntry { sig_id, stage_id, from: "dst" });
                    }
                }
                lex_vcs::MergeOutcome::Conflict { sig_id, kind, base, src, dst } => {
                    let kind: &'static str = match kind {
                        lex_vcs::ConflictKind::ModifyModify => "modify-modify",
                        lex_vcs::ConflictKind::ModifyDelete => "modify-delete",
                        lex_vcs::ConflictKind::DeleteModify => "delete-modify",
                        lex_vcs::ConflictKind::AddAdd       => "add-add",
                    };
                    report.conflicts.push(MergeConflict {
                        sig_id, kind, base, src, dst,
                    });
                }
            }
        }
        report.summary.clean = report.merged.len();
        report.summary.conflicts = report.conflicts.len();
        report.summary.total_sigs = report.merged.len() + report.conflicts.len();
        Ok(report)
    }

    pub fn commit_merge(&self, dst: &str, report: &MergeReport) -> Result<(), StoreError> {
        if !report.conflicts.is_empty() {
            return Err(StoreError::InvalidTransition(format!(
                "{} conflicts; resolve before committing", report.conflicts.len())));
        }
        let dst_head_map = self.branch_head(dst)?;
        let mut entries: BTreeMap<String, Option<String>> = BTreeMap::new();
        for m in &report.merged {
            let cur = dst_head_map.get(&m.sig_id);
            if cur != Some(&m.stage_id) {
                entries.insert(m.sig_id.clone(), Some(m.stage_id.clone()));
            }
        }
        let src_head = self.get_branch(&report.summary.src)?.and_then(|b| b.head_op);
        let dst_head_op = self.get_branch(dst)?.and_then(|b| b.head_op);

        match (src_head.clone(), dst_head_op.clone()) {
            // Fast-forward: dst is empty, just adopt src's head.
            (Some(s), None) => {
                self.set_branch_head_op(dst, s)?;
            }
            // Both sides have heads at the same op: nothing structural
            // to merge. Skip apply but still journal below.
            (Some(s), Some(d)) if s == d => { /* no-op */ }
            (Some(s), Some(d)) => {
                let op = lex_vcs::Operation::new(
                    lex_vcs::OperationKind::Merge { resolved: entries.len() },
                    [s, d],
                );
                let t = lex_vcs::StageTransition::Merge { entries };
                let _ = self.apply_operation(dst, op, t)?;
            }
            // src empty: nothing to merge in. Treat as no-op.
            (None, _) => { /* no-op */ }
        }

        // Atomicity note: the merge op is durable after apply_operation
        // returns; the journal entry below is a separate write. A
        // crash between leaves the merge in the op DAG but no journal
        // row — `lex log` will be missing this merge. The branch is
        // still functionally correct (head_op points at the merge op,
        // which carries `entries`), so the gap is recoverable by
        // re-running commit_merge once (which will journal but skip
        // the apply on the same-head match arm above). Tier-1 single-
        // writer assumption applies; multi-writer locking is on the
        // table for #130.

        // Journal the merge so `lex log` can show it.
        let mut b = self.get_branch(dst)?
            .ok_or_else(|| StoreError::UnknownBranch(dst.into()))?;
        if !report.summary.src.is_empty() {
            b.merges.push(MergeRecord {
                src: report.summary.src.clone(),
                at: now(),
                merged: report.merged.len(),
                conflicts: 0,
            });
            write_branch_atomic(&self.branch_path(dst), &b)?;
        }
        Ok(())
    }
}
