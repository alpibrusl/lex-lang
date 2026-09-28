//! Branches: each branch is identified by a name and a head OpId.
//! The SigId → StageId map every consumer reads is computed by
//! replaying the op log from the head back. No materialized cache.
//!
//! `lifecycle.json` (Draft/Active/Deprecated/Tombstone per stage)
//! survives as orthogonal stage-status metadata; it no longer drives
//! branch resolution.

use crate::files::{resolve_manifest_at, ManifestAt};
use crate::store::{Store, StoreError};
use lex_vcs::{OpId, OpLog, OperationRecord, StageTransition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

/// Why a CAS branch advance failed (#262). Surfaced through the
/// retry loop in `Store::apply_operation` and friends; callers
/// either retry (on `Mismatch`, after re-reading the head and
/// rebuilding the candidate op) or propagate (on `Io` /
/// `UnknownBranch`).
#[derive(Debug)]
pub enum CasFailed {
    /// Read-time `head_op` didn't match the supplied `expected`.
    /// Another writer advanced the branch between this caller's
    /// read and write. `actual` is the head we found instead.
    /// Public field so callers (e.g. an HTTP layer) can surface
    /// the actual head in a structured error envelope; today's
    /// retry loop just discards it and re-reads on the next
    /// iteration.
    #[allow(dead_code)] // populated for callers that inspect the variant
    Mismatch { actual: Option<OpId> },
    /// Branch doesn't exist (and isn't the default branch).
    UnknownBranch(String),
    /// Disk I/O failure (lock acquisition, file read, atomic
    /// write). Stringified at the boundary because `io::Error`
    /// isn't `Clone`/`PartialEq` and the variant is consumed by
    /// the retry loop, not pattern-matched on.
    Io(String),
}

/// Outcome of [`Store::advance_branch_head_ff`] — how the ref moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchAdvance {
    /// The branch didn't exist (or had no head) and was set to the new head.
    Created,
    /// The branch already pointed at the new head; nothing to do.
    UpToDate,
    /// The current head was an ancestor of the new head; advanced.
    FastForward,
}

pub const DEFAULT_BRANCH: &str = "main";

/// Current [`HeadSnapshot`] format/replay version (#1062).
const HEAD_SNAPSHOT_VERSION: u32 = 1;

/// Persisted, best-effort cache of `branch_head`'s computed view,
/// keyed on the head it was computed for. See `Store::branch_head`'s
/// doc comment for the incremental-replay design this backs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeadSnapshot {
    /// Which replay produced `map`. Snapshots written before #1062 lack it
    /// (deserialize as 0) and are discarded: the incremental extension they
    /// were built with mis-replayed merges, so a persisted merged head could
    /// be wrong, and reusing one would keep serving it.
    #[serde(default)]
    v: u32,
    head_op: OpId,
    map: BTreeMap<String, String>,
    /// The files manifest at `head_op` (#1007). `None` = not computed yet
    /// (a snapshot written before #1007, or by a `branch_head` call that had
    /// no cheap way to derive it); filled in lazily by `branch_manifest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    files: Option<ManifestAt>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Branch {
    pub name: String,
    pub parent: Option<String>,
    /// Op DAG head. `None` means the branch has never had an op
    /// applied (empty branch) *or* it's a predicate-defined branch
    /// where the head is computed lazily from `predicate`.
    #[serde(default)]
    pub head_op: Option<OpId>,
    /// Predicate over the op log (#133). When `Some`, the branch is
    /// a saved query rather than a snapshot — `head_op` is the
    /// optional materialization cache. The predicate's JSON shape
    /// matches `lex_vcs::Predicate::to_value()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<serde_json::Value>,
    /// Append-only journal of merges committed *into* this branch.
    #[serde(default)]
    pub merges: Vec<MergeRecord>,
    pub created_at: u64,
    /// Last op_id through which the producer-block gate (#248) has
    /// verified the branch's history (#256). When advancing from
    /// `head_op` to a new tip, the gate walks ops in
    /// `(last_gate_checkpoint .. head_op]` and runs the
    /// producer-block check on each ancestor's attestable stages —
    /// not just the new op. Once that walk passes, the checkpoint
    /// advances.
    ///
    /// Invalidated (set to `None`) when `lex attest retro-block`
    /// lands a new `ProducerBlock` attestation, forcing the next
    /// advance to re-walk from genesis once. Steady-state advances
    /// are `O(new ops)` because the previous advance already
    /// covered everything up through `last_gate_checkpoint`.
    ///
    /// Pre-#256 branch files have no `last_gate_checkpoint` field;
    /// serde defaults to `None`, which forces a one-time full walk
    /// on next advance. Same backward-compat trick `intent_id`
    /// (#131) used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gate_checkpoint: Option<OpId>,
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
    /// Sigs the merge decided to remove (a side deleted them and that
    /// deletion won). Kept separate from `merged` (which only carries
    /// present sig->stage) so `commit_merge` can propagate removals
    /// into the `Merge` transition's `entries` as `None` (#841). Was
    /// silently dropped before, so a src-side removal never reached
    /// dst via `commit_merge`.
    #[serde(default)]
    pub removed: Vec<String>,
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
                    // `branches/` also holds each branch's persisted head
                    // snapshot (`<branch>.head_snapshot.json`, see
                    // `save_head_snapshot`); its stem is not a branch name and
                    // its JSON is a SigId→StageId map, not a `Branch` — listing
                    // it would make `get_branch` fail on a phantom branch.
                    if name.ends_with(".head_snapshot") { continue; }
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

    fn head_snapshot_path(&self, name: &str) -> PathBuf {
        self.branches_dir().join(format!("{name}.head_snapshot.json"))
    }

    /// Best-effort read of the persisted snapshot for `name`. Any
    /// failure (missing file, corrupt/partial JSON from an unclean
    /// shutdown) is treated as "no snapshot" rather than an error —
    /// this is a pure performance optimization, so losing it must
    /// never break correctness, only fall back to a full walk.
    fn load_head_snapshot(&self, name: &str) -> Option<HeadSnapshot> {
        let raw = fs::read_to_string(self.head_snapshot_path(name)).ok()?;
        let snap: HeadSnapshot = serde_json::from_str(&raw).ok()?;
        (snap.v == HEAD_SNAPSHOT_VERSION).then_some(snap)
    }

    /// Best-effort write; a failure here (e.g. read-only filesystem)
    /// only costs a future full walk, so it's swallowed rather than
    /// propagated. Not atomic against a concurrent writer or a crash
    /// mid-write — same tradeoff `set_branch_head_op`'s own
    /// `fs::write` already makes for `branch_path`, and a torn write
    /// just fails `load_head_snapshot`'s parse on next read.
    fn save_head_snapshot(
        &self,
        name: &str,
        head_op: &OpId,
        map: &BTreeMap<String, String>,
        files: &Option<ManifestAt>,
    ) {
        let snap = HeadSnapshot {
            v: HEAD_SNAPSHOT_VERSION,
            head_op: head_op.clone(),
            map: map.clone(),
            files: files.clone(),
        };
        if let Ok(s) = serde_json::to_string(&snap) {
            let _ = fs::write(self.head_snapshot_path(name), s);
        }
    }

    /// Computed view: walk the op log from the branch head and
    /// replay each transition into a SigId → StageId map.
    ///
    /// Backed by a persisted snapshot (`<branch>.head_snapshot.json`)
    /// keyed on the head it was computed for. Steady state — this
    /// call's head_op matches the last call's — replays only the ops
    /// since the snapshot instead of the whole history: O(ops since
    /// the last call) instead of O(total branch history). Falls back
    /// to a full walk (and refreshes the snapshot) whenever there's no
    /// snapshot yet, or the snapshot's op isn't actually an ancestor
    /// of the new head (a branch reset, or history reordered by a
    /// merge) — see `OpLog::walk_forward_since`'s own doc comment.
    ///
    /// This existed as a genuine, measured bottleneck before the
    /// snapshot: a single call over a tenant with 110k+ accumulated
    /// ops took on the order of an hour, dominated by one disk read
    /// per ancestor op in the full BFS walk (alpibrusl/lex-lang#813's
    /// follow-up). Every consumer that used to call this once per
    /// file in a multi-file publish (fixed separately, also #813) now
    /// calls it once per publish request — but "once" was still a full
    /// walk over the *entire* history every time, since nothing
    /// persisted the result between calls.
    pub fn branch_head(&self, name: &str) -> Result<BTreeMap<String, String>, StoreError> {
        Ok(self.head_view(name, false)?.0)
    }

    /// The files manifest at `name`'s head (#1007) — see
    /// [`Store::manifest_at`]. Cached in the head snapshot alongside the
    /// sig→stage map and extended the same way, so steady state costs
    /// O(ops since the last call).
    pub fn branch_manifest(&self, name: &str) -> Result<ManifestAt, StoreError> {
        Ok(self.head_view(name, true)?.1.unwrap_or(ManifestAt::Absent))
    }

    /// `branch_head`'s map plus the head's files manifest. The manifest is
    /// computed whenever the records needed are already in memory (a full
    /// walk, or extending a snapshot that has it); otherwise only when
    /// `need_files`, so `branch_head` never pays an extra history walk.
    fn head_view(
        &self,
        name: &str,
        need_files: bool,
    ) -> Result<(BTreeMap<String, String>, Option<ManifestAt>), StoreError> {
        let b = match self.get_branch(name)? {
            Some(b) => b,
            None if name == DEFAULT_BRANCH => return Ok((BTreeMap::new(), Some(ManifestAt::Absent))),
            None => return Err(StoreError::UnknownBranch(name.into())),
        };
        let Some(head) = b.head_op else { return Ok((BTreeMap::new(), Some(ManifestAt::Absent))); };
        let log = OpLog::open(self.root())?;
        let files_from = |memo: &mut HashMap<OpId, ManifestAt>, recs: &[OperationRecord]| {
            let pre: HashMap<&str, &OperationRecord> =
                recs.iter().map(|r| (r.op_id.as_str(), r)).collect();
            resolve_manifest_at(&log, &head, memo, &pre)
        };

        if let Some(snap) = self.load_head_snapshot(name) {
            if snap.head_op == head {
                if snap.files.is_some() || !need_files {
                    return Ok((snap.map, snap.files));
                }
                let files = Some(files_from(&mut HashMap::new(), &[])?);
                self.save_head_snapshot(name, &head, &snap.map, &files);
                return Ok((snap.map, files));
            }
            // Extend the snapshot only when the ops since it are a pure
            // continuation of it. For a merge, `walk_forward_since` also
            // returns the merged-in branch's whole history — pre-fork ops
            // the snapshot already contains, possibly superseded since —
            // and replaying those on top of the snapshot reverts newer
            // changes (#1062). That case takes the full walk below.
            if let Some(new_records) = log
                .walk_forward_since(&head, &snap.head_op)?
                .filter(|recs| OpLog::continues_from(recs, &snap.head_op))
            {
                let new_records = OpLog::linearize(new_records);
                let mut map = snap.map;
                for rec in &new_records {
                    apply_transition(&mut map, &rec.produces);
                }
                let files = match snap.files {
                    Some(at_snap) => {
                        let mut memo = HashMap::from([(snap.head_op.clone(), at_snap)]);
                        Some(files_from(&mut memo, &new_records)?)
                    }
                    None if need_files => Some(files_from(&mut HashMap::new(), &new_records)?),
                    None => None,
                };
                self.save_head_snapshot(name, &head, &map, &files);
                return Ok((map, files));
            }
            // Snapshot's op isn't an ancestor of the new head — fall
            // through to a full walk below, which also refreshes it.
        }

        let mut map = BTreeMap::new();
        let records = log.walk_forward(&head, None)?;
        for rec in &records {
            apply_transition(&mut map, &rec.produces);
        }
        let files = Some(files_from(&mut HashMap::new(), &records)?);
        self.save_head_snapshot(name, &head, &map, &files);
        Ok((map, files))
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
            predicate: None,
            merges: Vec::new(),
            created_at: now(),
            last_gate_checkpoint: None,
        };
        fs::write(self.branch_path(name), serde_json::to_string_pretty(&b)?)?;
        Ok(())
    }

    /// Create a predicate-defined branch (#133). The branch's
    /// content is the set of ops matching `predicate`; `head_op`
    /// stays `None` and is materialized lazily by callers when
    /// they need a single point to apply ops against. Cheap to
    /// create and discard — it's a saved query, not a snapshot.
    pub fn create_predicate_branch(
        &self,
        name: &str,
        predicate: serde_json::Value,
    ) -> Result<(), StoreError> {
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            return Err(StoreError::InvalidTransition(
                format!("branch name `{name}` rejected (empty or path-like)")));
        }
        if self.branch_path(name).exists() {
            return Err(StoreError::InvalidTransition(
                format!("branch `{name}` already exists")));
        }
        fs::create_dir_all(self.branches_dir())?;
        let b = Branch {
            name: name.into(),
            parent: None,
            head_op: None,
            predicate: Some(predicate),
            merges: Vec::new(),
            created_at: now(),
            last_gate_checkpoint: None,
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
    /// Advance `branch` to `new_head`, fast-forward only — the ref half
    /// of `op push` (the op objects are transferred separately via the
    /// ops batch). Semantics mirror `git push` to a branch:
    ///
    /// * branch absent / no head yet → create it at `new_head`;
    /// * `new_head` already the head → no-op (`UpToDate`);
    /// * current head is an ancestor of `new_head` → fast-forward;
    /// * otherwise → [`StoreError::NonFastForward`], so a disjoint or
    ///   diverged history can't silently clobber a shared branch.
    ///
    /// `new_head` must already exist in the op log (the batch landed it);
    /// an unknown op is a `NonFastForward` against a head it can't reach.
    pub fn advance_branch_head_ff(
        &self,
        name: &str,
        new_head: &OpId,
    ) -> Result<BranchAdvance, StoreError> {
        let current = self.get_branch(name)?.and_then(|b| b.head_op);
        match current {
            None => {
                let history = lex_vcs::OpLog::open(self.root())?.walk_forward(new_head, None)?;
                self.check_head_satisfiable(&history)?;
                // First push to this branch (any name, not just main) —
                // create it pointing at new_head.
                let b = Branch {
                    name: name.to_string(),
                    parent: None,
                    head_op: Some(new_head.clone()),
                    predicate: None,
                    merges: Vec::new(),
                    created_at: now(),
                    last_gate_checkpoint: Some(new_head.clone()),
                };
                fs::create_dir_all(self.branches_dir())?;
                write_branch_atomic(&self.branch_path(name), &b)?;
                Ok(BranchAdvance::Created)
            }
            Some(cur) if &cur == new_head => Ok(BranchAdvance::UpToDate),
            Some(cur) => {
                // Fast-forward iff the current head is reachable from the
                // new head (i.e. an ancestor of it).
                let log = lex_vcs::OpLog::open(self.root())?;
                let history = log.walk_forward(new_head, None)?;
                let is_ff = history.iter().any(|rec| rec.op_id == cur);
                if is_ff {
                    self.check_head_satisfiable(&history)?;
                    self.set_branch_head_op(name, new_head.clone())?;
                    Ok(BranchAdvance::FastForward)
                } else {
                    Err(StoreError::NonFastForward {
                        branch: name.to_string(),
                        current: cur,
                        attempted: new_head.clone(),
                    })
                }
            }
        }
    }

    /// #992: the ref half of `op push` must not move a branch onto a head
    /// that names a `(sig, stage)` pair no store can hold. The op records
    /// arrive verbatim from the client — an older one can still carry a
    /// pre-#992 `ChangeEffectSig` `Replace` — so the hub cannot rely on the
    /// client having produced a valid head. Only the *head* is checked, not
    /// every pair history mentions: a historical pair is not rendered, and
    /// refusing it would make such a package unpushable forever (#999). A
    /// stage that is merely absent is not refused either — it proves nothing.
    ///
    /// `history` is `head_op`'s full forward walk, which the fast-forward
    /// test has already paid for.
    fn check_head_satisfiable(&self, history: &[lex_vcs::OperationRecord]) -> Result<(), StoreError> {
        let mut map = BTreeMap::new();
        for rec in history {
            apply_transition(&mut map, &rec.produces);
        }
        self.check_pairs_satisfiable(&map)
    }

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
                predicate: None,
                merges: Vec::new(),
                created_at: now(),
                last_gate_checkpoint: None,
            },
            None => return Err(StoreError::UnknownBranch(name.into())),
        };
        // #256: every successful advance also moves the gate
        // checkpoint to the new head. The gate that ran before this
        // call has already verified everything in
        // `(last_gate_checkpoint .. new_head]`, so the new head is
        // now the verified frontier.
        b.head_op = Some(head_op.clone());
        b.last_gate_checkpoint = Some(head_op);
        fs::create_dir_all(self.branches_dir())?;
        write_branch_atomic(&self.branch_path(name), &b)?;
        Ok(())
    }

    /// Atomic compare-and-swap on `branch.head_op` (#262). Holds
    /// an advisory `flock` on a per-branch lock file across the
    /// read-compare-write sequence so two concurrent writers can't
    /// both see the same `head_op`, both decide to advance, and
    /// both succeed (silently dropping one's lineage).
    ///
    /// Returns `Ok(())` when `expected == current head_op` and the
    /// new value is durably written; `Err(CasFailed { actual })`
    /// when the actual head doesn't match `expected`. Callers
    /// should re-read the head, rebuild the candidate op against
    /// the new parent, and retry. Op records are content-addressed
    /// and idempotent, so re-persisting under a new parent is safe.
    ///
    /// Crash safety: same tempfile + rename + fsync as the
    /// non-CAS path. The lock is the only addition; on a crashed
    /// process the OS releases the flock and the next writer can
    /// proceed.
    pub(crate) fn set_branch_head_op_cas(
        &self,
        name: &str,
        expected: Option<OpId>,
        new: OpId,
    ) -> Result<(), CasFailed> {
        // Acquire the per-branch advisory lock. Path is
        // `<branches_dir>/<name>.lock`; the file is created on
        // first use and reused thereafter. We hold the lock for
        // the entire RMW sequence.
        fs::create_dir_all(self.branches_dir())
            .map_err(|e| CasFailed::Io(e.to_string()))?;
        let lock_path = self.branches_dir().join(format!("{name}.lock"));
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| CasFailed::Io(e.to_string()))?;
        use fs2::FileExt;
        lock_file.lock_exclusive()
            .map_err(|e| CasFailed::Io(e.to_string()))?;

        // Critical section: read, compare, write. The lock is
        // released when `lock_file` drops at end of scope (or on
        // early return).
        let result = (|| -> Result<(), CasFailed> {
            let actual = self.get_branch(name)
                .map_err(|e| CasFailed::Io(format!("{e}")))?
                .and_then(|b| b.head_op);
            if actual != expected {
                return Err(CasFailed::Mismatch { actual });
            }
            let mut b = match self.get_branch(name)
                .map_err(|e| CasFailed::Io(format!("{e}")))?
            {
                Some(b) => b,
                None if name == DEFAULT_BRANCH => Branch {
                    name: DEFAULT_BRANCH.into(),
                    parent: None,
                    head_op: None,
                    predicate: None,
                    merges: Vec::new(),
                    created_at: now(),
                    last_gate_checkpoint: None,
                },
                None => return Err(CasFailed::UnknownBranch(name.into())),
            };
            b.head_op = Some(new.clone());
            b.last_gate_checkpoint = Some(new);
            write_branch_atomic(&self.branch_path(name), &b)
                .map_err(|e| CasFailed::Io(format!("{e}")))?;
            Ok(())
        })();
        // Best-effort unlock; OS releases on file close anyway.
        let _ = fs2::FileExt::unlock(&lock_file);
        result
    }

    /// Invalidate every branch's `last_gate_checkpoint` (#256). Run
    /// when a new `ProducerBlock` attestation lands so the next
    /// branch advance walks back from genesis once and re-verifies
    /// the full chain. Returns the number of branches whose
    /// checkpoint changed.
    pub fn invalidate_gate_checkpoints(&self) -> Result<usize, StoreError> {
        let dir = self.branches_dir();
        if !dir.exists() {
            return Ok(0);
        }
        let mut updated = 0usize;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") { continue; }
            let bytes = fs::read(&path)?;
            let mut b: Branch = match serde_json::from_slice(&bytes) {
                Ok(b) => b,
                // Corrupt branch file shouldn't take down the
                // invalidation pass; the next gate run will surface
                // the parse error on a real call path.
                Err(_) => continue,
            };
            if b.last_gate_checkpoint.is_some() {
                b.last_gate_checkpoint = None;
                write_branch_atomic(&path, &b)?;
                updated += 1;
            }
        }
        Ok(updated)
    }
}

/// Apply a single `StageTransition` to a sig-stage map. Used by
/// `branch_head` to replay an op log.
pub(crate) fn apply_transition(map: &mut BTreeMap<String, String>, t: &StageTransition) {
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
        StageTransition::ImportOnly | StageTransition::FilesOnly => {}
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
            removed: Vec::new(),
        };
        for o in out.outcomes {
            match o {
                lex_vcs::MergeOutcome::Both { sig_id, stage_id } => {
                    if let Some(stage_id) = stage_id {
                        report.merged.push(MergeEntry { sig_id, stage_id, from: "both" });
                    }
                }
                lex_vcs::MergeOutcome::Src { sig_id, stage_id } => match stage_id {
                    Some(stage_id) => report.merged.push(MergeEntry { sig_id, stage_id, from: "src" }),
                    None => report.removed.push(sig_id),
                },
                lex_vcs::MergeOutcome::Dst { sig_id, stage_id } => match stage_id {
                    Some(stage_id) => report.merged.push(MergeEntry { sig_id, stage_id, from: "dst" }),
                    None => report.removed.push(sig_id),
                },
                lex_vcs::MergeOutcome::Conflict { sig_id, kind, base: base_stage, src: src_stage, dst: dst_stage } => {
                    // #838: a ModifyModify where both sides edited the
                    // same function is not necessarily a conflict — if
                    // the edits touch disjoint subtrees (different match
                    // arms, different let bindings) and the merged body
                    // type-checks, compose them into a new typed stage
                    // instead. `ours` = dst side, `theirs` = src side.
                    // (`dst` here is the destination branch name.)
                    if let (lex_vcs::ConflictKind::ModifyModify, Some(b), Some(s), Some(d)) =
                        (&kind, &base_stage, &src_stage, &dst_stage)
                    {
                        if let Some(merged_id) =
                            self.try_semantic_body_merge(dst, &sig_id, b, d, s)?
                        {
                            report.merged.push(MergeEntry {
                                sig_id,
                                stage_id: merged_id,
                                from: "semantic",
                            });
                            continue;
                        }
                    }
                    let kind: &'static str = match kind {
                        lex_vcs::ConflictKind::ModifyModify => "modify-modify",
                        lex_vcs::ConflictKind::ModifyDelete => "modify-delete",
                        lex_vcs::ConflictKind::DeleteModify => "delete-modify",
                        lex_vcs::ConflictKind::AddAdd       => "add-add",
                    };
                    report.conflicts.push(MergeConflict {
                        sig_id, kind, base: base_stage, src: src_stage, dst: dst_stage,
                    });
                }
            }
        }
        report.summary.clean = report.merged.len();
        report.summary.conflicts = report.conflicts.len();
        report.summary.total_sigs = report.merged.len() + report.conflicts.len();
        Ok(report)
    }

    /// The SigId → StageId map at `op_id`: the full replay of that op's
    /// ancestry in [`OpLog::walk_forward`]'s canonical order, computed fresh
    /// (never through a branch's incremental snapshot). This is the single
    /// definition of "the head at an op"; `branch_head` must agree with it.
    pub fn sig_map_at_op(&self, op_id: &str) -> Result<BTreeMap<String, String>, StoreError> {
        let log = OpLog::open(self.root())?;
        let mut map = BTreeMap::new();
        for rec in log.walk_forward(&op_id.to_string(), None)? {
            apply_transition(&mut map, &rec.produces);
        }
        Ok(map)
    }

    /// The `Merge` entries that pin every sig a merge **decided** to the
    /// value it decided, so the merged head is a function of the resolved
    /// merge and not of the order the two parallel histories are replayed in
    /// (#1062).
    ///
    /// A merge op is replayed by re-applying both parents' ancestries and
    /// then its own entries. Both sides' ops on a sig the merge decided are
    /// concurrent and do not commute (a rename retires the sig a modify
    /// rebinds), so whichever the replay happens to apply last used to win —
    /// and a resolution that leaves the sig as dst already has it (`take_ours`,
    /// or a sig only dst touched) changes nothing relative to dst, so it was
    /// never listed and nothing outranked the replay. Listing it here does.
    ///
    /// * `Src` outcomes: src's stage (`None`: src removed it).
    /// * `Dst` / `Both` outcomes and `TakeOurs`: the sig as dst's head has it
    ///   (`None`: dst lacks it, so the merge keeps it absent).
    /// * `TakeTheirs`: the sig as src's head has it.
    ///
    /// `Custom` resolutions carry their target in the op itself; callers add
    /// them on top (and own the error for an op that has none).
    pub fn merge_pins(
        &self,
        dst_head: Option<&OpId>,
        src_head: Option<&OpId>,
        auto_resolved: &[lex_vcs::MergeOutcome],
        resolved: &[(lex_vcs::ConflictId, lex_vcs::Resolution)],
    ) -> Result<BTreeMap<String, Option<String>>, StoreError> {
        let map_at = |h: Option<&OpId>| match h {
            Some(h) => self.sig_map_at_op(h),
            None => Ok(BTreeMap::new()),
        };
        let dst = map_at(dst_head)?;
        let src = if resolved.iter().any(|(_, r)| matches!(r, lex_vcs::Resolution::TakeTheirs)) {
            map_at(src_head)?
        } else {
            BTreeMap::new()
        };
        let mut entries: BTreeMap<String, Option<String>> = BTreeMap::new();
        for outcome in auto_resolved {
            match outcome {
                lex_vcs::MergeOutcome::Src { sig_id, stage_id } => {
                    entries.insert(sig_id.clone(), stage_id.clone());
                }
                lex_vcs::MergeOutcome::Dst { sig_id, .. }
                | lex_vcs::MergeOutcome::Both { sig_id, .. } => {
                    entries.insert(sig_id.clone(), dst.get(sig_id).cloned());
                }
                lex_vcs::MergeOutcome::Conflict { .. } => {}
            }
        }
        for (sig, resolution) in resolved {
            match resolution {
                lex_vcs::Resolution::TakeOurs => {
                    entries.insert(sig.clone(), dst.get(sig).cloned());
                }
                lex_vcs::Resolution::TakeTheirs => {
                    entries.insert(sig.clone(), src.get(sig).cloned());
                }
                lex_vcs::Resolution::Custom { .. } | lex_vcs::Resolution::Defer => {}
            }
        }
        Ok(entries)
    }

    pub fn commit_merge(&self, dst: &str, report: &MergeReport) -> Result<(), StoreError> {
        if !report.conflicts.is_empty() {
            return Err(StoreError::InvalidTransition(format!(
                "{} conflicts; resolve before committing", report.conflicts.len())));
        }
        let dst_head_map = self.branch_head(dst)?;
        // #1062: pin every sig the merge decided, not only the ones that
        // differ from dst — see `merge_pins`. What dst already has is pinned
        // to dst's own value.
        let mut entries: BTreeMap<String, Option<String>> = BTreeMap::new();
        for m in &report.merged {
            let value = match m.from {
                "dst" | "both" => dst_head_map.get(&m.sig_id).cloned(),
                _ => Some(m.stage_id.clone()),
            };
            entries.insert(m.sig_id.clone(), value);
        }
        // #841: propagate removals the merge decided on. A sig dst lacks is
        // pinned absent too, so a replay cannot resurrect it (#1062).
        for sig in &report.removed {
            entries.insert(sig.clone(), None);
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
                // Git convention: first parent is the branch being merged
                // INTO (dst), second is the one merged in (src). The HTTP
                // and CLI commit handlers already use this order; #841
                // aligns commit_merge so the same merge yields the same
                // head on every path.
                let op = lex_vcs::Operation::new(
                    lex_vcs::OperationKind::Merge { resolved: entries.len() },
                    [d, s],
                );
                let t = lex_vcs::StageTransition::Merge { entries };
                // Gated (#833): land the merge op, type-check the real
                // post-merge head, and roll back if it doesn't compose.
                let _ = self.apply_merge_op_gated(dst, op, t)?;
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

#[cfg(test)]
mod branch_head_snapshot_tests {
    use super::*;
    use lex_vcs::{Operation, OperationKind};
    use std::collections::BTreeSet;

    fn add(store: &Store, sig: &str, stg: &str) -> OpId {
        let parent = store.get_branch(DEFAULT_BRANCH).unwrap().and_then(|b| b.head_op);
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stg.into(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            parent.into_iter().collect::<Vec<_>>(),
        );
        let transition = StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() };
        store.apply_operation(DEFAULT_BRANCH, op, transition).unwrap()
    }

    /// The fallback this exercises can't be reached through the public
    /// API alone: `apply_operation`'s CAS retry always rebuilds a
    /// single-parent op's `parents` to match the *current* head, so
    /// there is no ordinary way to advance a branch to an op that
    /// doesn't descend from its own history. `set_branch_head_op`
    /// (crate-internal) is what a real reset/rebase operation would
    /// eventually call, so this directly forces that same shape: a
    /// head whose ancestry does NOT include the op the persisted
    /// snapshot was computed for.
    #[test]
    fn branch_head_falls_back_to_full_walk_when_snapshot_predates_a_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();

        add(&store, "fn::a", "stage_a");
        add(&store, "fn::b", "stage_b");
        let snapshotted = store.branch_head(DEFAULT_BRANCH).unwrap();
        assert_eq!(snapshotted.len(), 2, "sanity: snapshot covers both ops");

        // Force the branch onto a disconnected, single-op history —
        // the snapshot's op is not among its ancestors.
        let reset_op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fn::reset_only".into(),
                stage_id: "stage_reset".into(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            Vec::new(), // no parents: a fresh root, unrelated to fn::a/fn::b
        );
        let reset_op_id = reset_op.op_id();
        let reset_record = lex_vcs::OperationRecord::new(
            reset_op,
            StageTransition::Create {
                sig_id: "fn::reset_only".into(),
                stage_id: "stage_reset".into(),
            },
        );
        let log = OpLog::open(store.root()).unwrap();
        log.put(&reset_record).unwrap();
        store.set_branch_head_op(DEFAULT_BRANCH, reset_op_id).unwrap();

        let after_reset = store.branch_head(DEFAULT_BRANCH).unwrap();
        assert_eq!(
            after_reset.len(), 1,
            "stale snapshot must not be reused across a non-ancestor head change: {after_reset:?}"
        );
        assert_eq!(after_reset.get("fn::reset_only"), Some(&"stage_reset".to_string()));
        assert!(!after_reset.contains_key("fn::a"));
        assert!(!after_reset.contains_key("fn::b"));

        // A repeat call must now hit the (correctly refreshed) snapshot
        // and still agree.
        let again = store.branch_head(DEFAULT_BRANCH).unwrap();
        assert_eq!(after_reset, again);
    }

    #[test]
    fn advance_branch_head_ff_creates_advances_and_refuses_nonff() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        // Chain a <- b <- c on main (the object DAG a push would transfer).
        let _a = add(&store, "fn::a", "sa");
        let b = add(&store, "fn::b", "sb");
        let c = add(&store, "fn::c", "sc");

        // First push to a fresh branch creates it at the pushed head.
        assert_eq!(store.advance_branch_head_ff("feat", &c).unwrap(), BranchAdvance::Created);
        assert_eq!(store.get_branch("feat").unwrap().unwrap().head_op, Some(c.clone()));

        // Re-pushing the same head is a no-op.
        assert_eq!(store.advance_branch_head_ff("feat", &c).unwrap(), BranchAdvance::UpToDate);

        // Reset feat to b; advancing to c fast-forwards (b is c's ancestor).
        store.set_branch_head_op("feat", b.clone()).unwrap();
        assert_eq!(store.advance_branch_head_ff("feat", &c).unwrap(), BranchAdvance::FastForward);

        // A disjoint root is a non-fast-forward and must be refused, leaving
        // the branch untouched (the clobber-prevention the alpibrusl push hit).
        let d_op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fn::d".into(),
                stage_id: "sd".into(),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            },
            Vec::new(),
        );
        let d = d_op.op_id();
        OpLog::open(store.root())
            .unwrap()
            .put(&lex_vcs::OperationRecord::new(
                d_op,
                StageTransition::Create { sig_id: "fn::d".into(), stage_id: "sd".into() },
            ))
            .unwrap();
        match store.advance_branch_head_ff("feat", &d) {
            Err(StoreError::NonFastForward { .. }) => {}
            other => panic!("expected NonFastForward, got {other:?}"),
        }
        assert_eq!(store.get_branch("feat").unwrap().unwrap().head_op, Some(c));
    }
}
