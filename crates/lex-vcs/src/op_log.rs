//! Persistence + DAG queries for the operation log.
//!
//! Layout: `<root>/ops/<op_id>.json` — one canonical-JSON file
//! per [`OperationRecord`]. Atomic writes via tempfile + rename.
//! Idempotent: writing an existing op_id is a no-op (content
//! addressing guarantees the bytes match).
//!
//! # Packfiles (#261 slice 1)
//!
//! Loose-file storage is fine to ~10k ops; past that the
//! filesystem starts to thrash. [`OpLog::repack`] consolidates
//! loose files into deterministic, content-addressed packfiles:
//!
//! - `<dir>/pack-<hash>.pack`: each record framed as `[8-byte BE
//!   length][canonical JSON]`, ops sorted by op_id within the pack.
//! - `<dir>/pack-<hash>.idx`: JSON map of `op_id` → byte offset
//!   into the `.pack` (offset of the length header).
//!
//! Pack name is the SHA-256 of the sorted op_ids, newline-joined,
//! so the same input set always produces the same pack hash —
//! a re-run of `lex op repack` is a no-op.
//!
//! [`OpLog::get`] tries loose first, falls back to scanning all
//! `.idx` files in the directory. The write path
//! ([`OpLog::put`]) only ever writes loose; ops migrate into
//! packs via the explicit [`OpLog::repack`] call.

use crate::canonical::hash_bytes;
use crate::operation::{OpId, OperationRecord};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct OpLog {
    dir: PathBuf,
}

impl OpLog {
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join("ops");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, op_id: &OpId) -> PathBuf {
        self.dir.join(format!("{op_id}.json"))
    }

    /// Persist a record. Idempotent on existing op_ids (the bytes
    /// must match by content addressing).
    ///
    /// Crash safety: the tempfile's data is fsync'd before rename,
    /// so a successful return implies a durable file at the final
    /// path. The containing directory is not fsync'd; on a crash
    /// between rename and the directory's metadata flush, the file
    /// can be lost. For a content-addressed log this is acceptable
    /// — a lost record can be re-derived from the same source — but
    /// callers that *also* persist references to the op_id (e.g.
    /// branch heads) should fsync those refs after `put` returns.
    pub fn put(&self, rec: &OperationRecord) -> io::Result<()> {
        let path = self.path(&rec.op_id);
        if path.exists() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(rec)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, op_id: &OpId) -> io::Result<Option<OperationRecord>> {
        let path = self.path(op_id);
        if path.exists() {
            let bytes = fs::read(&path)?;
            let rec: OperationRecord = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            return Ok(Some(rec));
        }
        // Loose miss — scan packfiles. Each `.idx` is a tiny JSON
        // map; small constant cost per pack. For larger stores we
        // could maintain an in-memory cache keyed off pack mtimes,
        // but slice 1 keeps it simple — measure before optimizing.
        for pack_idx in self.list_pack_indices()? {
            let idx = PackIndex::load(&pack_idx)?;
            if let Some(&offset) = idx.ops.get(op_id) {
                let pack_path = pack_idx.with_extension("pack");
                return read_packed_op(&pack_path, offset).map(Some);
            }
        }
        Ok(None)
    }

    /// Walk the directory for `pack-*.idx` files. Order is whatever
    /// the filesystem gives us — `get` doesn't depend on it (op_ids
    /// are unique by content addressing, so the right pack wins).
    fn list_pack_indices(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if name.starts_with("pack-") && name.ends_with(".idx") {
                out.push(entry.path());
            }
        }
        Ok(out)
    }

    /// Consolidate loose op records into a deterministic, content-
    /// addressed packfile (#261 slice 1). Returns the number of
    /// ops moved into the new pack.
    ///
    /// `threshold` is the minimum number of loose ops required to
    /// trigger a repack — under that, returns `0` and leaves the
    /// log alone. The idea: small stores stay loose; only repack
    /// when the file count starts to matter.
    ///
    /// Determinism: the pack name is the SHA-256 of the sorted
    /// op_ids (newline-joined), so two independent runs against the
    /// same set of loose ops produce a byte-identical pack.
    /// Re-running on an empty loose directory is a no-op.
    ///
    /// Crash safety: the `.pack.tmp` and `.idx.tmp` files are
    /// fsync'd before rename; loose files are deleted only after
    /// both renames succeed. A crash mid-repack leaves both loose
    /// and partial-pack files; a subsequent `get` finds the loose
    /// version, and a subsequent `repack` cleans up.
    pub fn repack(&self, threshold: usize) -> io::Result<usize> {
        let loose: Vec<(OpId, PathBuf)> = self.list_loose_files()?;
        if loose.len() < threshold {
            return Ok(0);
        }
        // Sort ops deterministically by op_id (lex order). The pack
        // hash is the SHA-256 of those op_ids joined by newlines —
        // same input → same name.
        let mut ops: Vec<(OpId, Vec<u8>)> = Vec::with_capacity(loose.len());
        for (op_id, path) in &loose {
            let bytes = fs::read(path)?;
            ops.push((op_id.clone(), bytes));
        }
        ops.sort_by(|a, b| a.0.cmp(&b.0));
        let mut name_input = Vec::new();
        for (id, _) in &ops {
            name_input.extend_from_slice(id.as_bytes());
            name_input.push(b'\n');
        }
        let pack_hash = hash_bytes(&name_input);
        let pack_path = self.dir.join(format!("pack-{pack_hash}.pack"));
        let idx_path = self.dir.join(format!("pack-{pack_hash}.idx"));
        if pack_path.exists() && idx_path.exists() {
            // Same input set — pack already exists. Just clean up
            // the loose duplicates.
            let count = ops.len();
            for (_, path) in &loose {
                let _ = fs::remove_file(path);
            }
            return Ok(count);
        }

        // Write `<pack>.pack.tmp` framed as [8-byte BE length][JSON]
        // for each record; record offsets for the index.
        let pack_tmp = pack_path.with_extension("pack.tmp");
        let idx_tmp = idx_path.with_extension("idx.tmp");
        let mut offsets: BTreeMap<OpId, u64> = BTreeMap::new();
        {
            let mut f = fs::File::create(&pack_tmp)?;
            let mut cursor: u64 = 0;
            for (op_id, bytes) in &ops {
                offsets.insert(op_id.clone(), cursor);
                let len = bytes.len() as u64;
                f.write_all(&len.to_be_bytes())?;
                f.write_all(bytes)?;
                cursor += 8 + len;
            }
            f.sync_all()?;
        }
        // Write the index. JSON for inspectability and
        // forward-compat (we can add fields without breaking
        // readers).
        let idx = PackIndex { version: 1, ops: offsets };
        idx.save(&idx_tmp)?;

        fs::rename(&pack_tmp, &pack_path)?;
        fs::rename(&idx_tmp, &idx_path)?;

        // Now safe to delete the loose files — pack is durable.
        let count = ops.len();
        for (_, path) in &loose {
            let _ = fs::remove_file(path);
        }
        Ok(count)
    }

    /// Enumerate every loose `<op_id>.json` in the ops directory.
    /// Used by [`Self::repack`] and [`Self::list_all`].
    fn list_loose_files(&self) -> io::Result<Vec<(OpId, PathBuf)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(id) = name.strip_suffix(".json") {
                if !id.starts_with("pack-") {
                    out.push((id.to_string(), entry.path()));
                }
            }
        }
        Ok(out)
    }

    /// Remove a record from the log. Used by [`crate::migrate`] to
    /// delete the old `<op_id>.json` files after a format migration
    /// has written their replacements. Idempotent on missing files.
    ///
    /// **Not** part of the day-to-day op-log API — the log is
    /// append-only by design (#129). The only legitimate caller is
    /// the migration tool, which is supervising a destructive,
    /// `--confirm`-gated batch.
    pub fn delete(&self, op_id: &OpId) -> io::Result<()> {
        let path = self.path(op_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Walk parents transitively. Newest-first, BFS, dedup'd by op_id.
    /// Stops at parentless ops or after `limit` records.
    pub fn walk_back(
        &self,
        head: &OpId,
        limit: Option<usize>,
    ) -> io::Result<Vec<OperationRecord>> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut frontier: VecDeque<OpId> = VecDeque::from([head.clone()]);
        while let Some(id) = frontier.pop_back() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(rec) = self.get(&id)? {
                // Push parents before recording so traversal order is
                // a stable BFS-by-discovery: children-first, then their
                // parents, parents of those, etc.
                for p in &rec.op.parents {
                    if !seen.contains(p) {
                        frontier.push_front(p.clone());
                    }
                }
                out.push(rec);
                if let Some(n) = limit {
                    if out.len() >= n {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// Same set as walk_back but oldest-first. Used by branch_head
    /// for left-to-right transition replay.
    pub fn walk_forward(
        &self,
        head: &OpId,
        limit: Option<usize>,
    ) -> io::Result<Vec<OperationRecord>> {
        let mut all = self.walk_back(head, None)?;
        all.reverse();
        if let Some(n) = limit {
            all.truncate(n);
        }
        Ok(all)
    }

    /// Common ancestor of two op_ids in the DAG.
    ///
    /// On tree-shaped histories and chain merges this is the
    /// **lowest** common ancestor — the closest shared op. On
    /// criss-cross merges (two ops each with two parents from
    /// independent histories) there can be multiple
    /// incomparable common ancestors; this picks one
    /// deterministically (the first hit when traversing `b`'s
    /// ancestors newest-first), but not via a recursive merge.
    /// `None` if no shared ancestor exists.
    ///
    /// Tier-1 merge in #129 covers linear and tree-shaped
    /// histories; criss-cross resolution is deferred to a
    /// future tier (Git's `recursive` strategy is the reference).
    pub fn lca(&self, a: &OpId, b: &OpId) -> io::Result<Option<OpId>> {
        let a_anc: BTreeSet<OpId> = self
            .walk_back(a, None)?
            .into_iter()
            .map(|r| r.op_id)
            .collect();
        // Walk b's ancestors newest-first; first hit is the deepest
        // common ancestor on tree-shaped histories. In criss-cross
        // DAGs this picks deterministically but not via recursive
        // resolution — see the doc comment above.
        for rec in self.walk_back(b, None)? {
            if a_anc.contains(&rec.op_id) {
                return Ok(Some(rec.op_id));
            }
        }
        Ok(None)
    }

    /// Every record in the log. Order is whatever the directory
    /// listing produces — undefined and not stable. Used by the
    /// [`crate::predicate`] evaluator when no narrower candidate
    /// set is available.
    pub fn list_all(&self) -> io::Result<Vec<OperationRecord>> {
        let mut out = Vec::new();
        let mut seen: BTreeSet<OpId> = BTreeSet::new();
        // Loose first so dedup wins for them on collision (loose
        // and pack should never both exist for the same op_id post-
        // repack, but during an interrupted repack both can be
        // present transiently).
        for (id, _) in self.list_loose_files()? {
            if let Some(rec) = self.get(&id)? {
                if seen.insert(rec.op_id.clone()) {
                    out.push(rec);
                }
            }
        }
        for pack_idx in self.list_pack_indices()? {
            let idx = PackIndex::load(&pack_idx)?;
            let pack_path = pack_idx.with_extension("pack");
            for (op_id, &offset) in &idx.ops {
                if seen.insert(op_id.clone()) {
                    out.push(read_packed_op(&pack_path, offset)?);
                }
            }
        }
        Ok(out)
    }

    /// Ops in `head`'s history that are not in `base`'s history.
    /// `base = None` means "include all of head's history" (used for
    /// independent-histories case where the LCA is None).
    pub fn ops_since(
        &self,
        head: &OpId,
        base: Option<&OpId>,
    ) -> io::Result<Vec<OperationRecord>> {
        let exclude: BTreeSet<OpId> = match base {
            Some(b) => self
                .walk_back(b, None)?
                .into_iter()
                .map(|r| r.op_id)
                .collect(),
            None => BTreeSet::new(),
        };
        Ok(self
            .walk_back(head, None)?
            .into_iter()
            .filter(|r| !exclude.contains(&r.op_id))
            .collect())
    }
}

/// Sidecar index for a packfile. Maps `op_id` to the byte offset
/// of the record's length header inside the `.pack`. JSON for
/// inspectability and forward-compat.
#[derive(serde::Serialize, serde::Deserialize)]
struct PackIndex {
    version: u32,
    ops: BTreeMap<OpId, u64>,
}

impl PackIndex {
    fn load(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut f = fs::File::create(path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(())
    }
}

/// Read one record from a packfile at `offset`. The record is
/// framed as `[8-byte BE length][canonical JSON]`.
fn read_packed_op(pack_path: &Path, offset: u64) -> io::Result<OperationRecord> {
    let mut f = fs::File::open(pack_path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut len_buf = [0u8; 8];
    f.read_exact(&mut len_buf)?;
    let len = u64::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{Operation, OperationKind, StageTransition};
    use std::collections::{BTreeMap, BTreeSet};

    fn add_op() -> OperationRecord {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac::Int->Int".into(),
                stage_id: "abc123".into(),
                effects: BTreeSet::new(),
                budget_cost: None,
            },
            [],
        );
        OperationRecord::new(
            op,
            StageTransition::Create {
                sig_id: "fac::Int->Int".into(),
                stage_id: "abc123".into(),
            },
        )
    }

    fn modify_op(parent: &OpId, sig: &str, from: &str, to: &str) -> OperationRecord {
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
                from_budget: None,
                to_budget: None,
            },
            [parent.clone()],
        );
        OperationRecord::new(
            op,
            StageTransition::Replace {
                sig_id: sig.into(),
                from: from.into(),
                to: to.into(),
            },
        )
    }

    #[test]
    fn put_then_get_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let rec = add_op();
        log.put(&rec).unwrap();
        let back = log.get(&rec.op_id).unwrap().unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn put_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let rec = add_op();
        log.put(&rec).unwrap();
        log.put(&rec).unwrap(); // second write is a no-op
        assert!(log.get(&rec.op_id).unwrap().is_some());
    }

    #[test]
    fn get_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        assert!(log.get(&"deadbeef".to_string()).unwrap().is_none());
    }

    #[test]
    fn walk_back_returns_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "def456");
        log.put(&b).unwrap();
        let c = modify_op(&b.op_id, "fac::Int->Int", "def456", "789aaa");
        log.put(&c).unwrap();

        let walked = log.walk_back(&c.op_id, None).unwrap();
        let ids: Vec<_> = walked.iter().map(|r| r.op_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![c.op_id.as_str(), b.op_id.as_str(), a.op_id.as_str()]
        );
    }

    #[test]
    fn walk_forward_returns_oldest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "def456");
        log.put(&b).unwrap();

        let walked = log.walk_forward(&b.op_id, None).unwrap();
        let ids: Vec<_> = walked.iter().map(|r| r.op_id.as_str()).collect();
        assert_eq!(ids, vec![a.op_id.as_str(), b.op_id.as_str()]);
    }

    #[test]
    fn lca_finds_common_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let root = add_op();
        log.put(&root).unwrap();
        let left = modify_op(&root.op_id, "fac::Int->Int", "abc123", "left1");
        log.put(&left).unwrap();
        let right = modify_op(&root.op_id, "fac::Int->Int", "abc123", "right1");
        log.put(&right).unwrap();

        let lca = log.lca(&left.op_id, &right.op_id).unwrap();
        assert_eq!(lca, Some(root.op_id));
    }

    #[test]
    fn lca_none_for_independent_histories() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        // A second parentless op (different sig, so different op_id).
        let b = OperationRecord::new(
            Operation::new(
                OperationKind::AddFunction {
                    sig_id: "double::Int->Int".into(),
                    stage_id: "ddd111".into(),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                },
                [],
            ),
            StageTransition::Create {
                sig_id: "double::Int->Int".into(),
                stage_id: "ddd111".into(),
            },
        );
        log.put(&b).unwrap();

        assert_eq!(log.lca(&a.op_id, &b.op_id).unwrap(), None);
    }

    #[test]
    fn ops_since_excludes_base_history() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "def456");
        log.put(&b).unwrap();
        let c = modify_op(&b.op_id, "fac::Int->Int", "def456", "789aaa");
        log.put(&c).unwrap();

        let since: Vec<_> = log
            .ops_since(&c.op_id, Some(&a.op_id))
            .unwrap()
            .into_iter()
            .map(|r| r.op_id)
            .collect();
        assert_eq!(since.len(), 2);
        assert!(since.contains(&b.op_id));
        assert!(since.contains(&c.op_id));
        assert!(!since.contains(&a.op_id));
    }

    #[test]
    fn repack_consolidates_loose_files_into_a_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "def456");
        log.put(&b).unwrap();

        let n = log.repack(0).unwrap();  // threshold 0 = always
        assert_eq!(n, 2);
        let ops_dir = tmp.path().join("ops");
        let loose: Vec<_> = fs::read_dir(&ops_dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter(|e| !e.file_name().to_string_lossy().starts_with("pack-"))
            .collect();
        assert!(loose.is_empty(), "loose .json files should be deleted");
        let packs: Vec<_> = fs::read_dir(&ops_dir).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(packs.len(), 1);

        // After repack, get() must still return both ops via the pack.
        assert_eq!(log.get(&a.op_id).unwrap().unwrap(), a);
        assert_eq!(log.get(&b.op_id).unwrap().unwrap(), b);
    }

    #[test]
    fn repack_below_threshold_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        log.put(&add_op()).unwrap();
        let n = log.repack(10).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn repack_is_deterministic_on_same_input() {
        // Two stores with the same loose ops repack to the same
        // pack hash — content addressing all the way down.
        let make_log = || {
            let tmp = tempfile::tempdir().unwrap();
            let log = OpLog::open(tmp.path()).unwrap();
            let a = add_op();
            log.put(&a).unwrap();
            let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "def456");
            log.put(&b).unwrap();
            log.repack(0).unwrap();
            (tmp, log)
        };
        let (tmp1, _log1) = make_log();
        let (tmp2, _log2) = make_log();
        let pack_name = |dir: &std::path::Path| -> String {
            fs::read_dir(dir.join("ops")).unwrap()
                .filter_map(|e| e.ok())
                .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
                .unwrap()
                .file_name().into_string().unwrap()
        };
        assert_eq!(pack_name(tmp1.path()), pack_name(tmp2.path()));
    }

    #[test]
    fn walk_back_works_across_loose_and_packed_ops() {
        // Pack the older history, leave newer ops loose. walk_back
        // must traverse seamlessly.
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "b1");
        log.put(&b).unwrap();
        log.repack(0).unwrap();
        // Now add a newer op as a loose file.
        let c = modify_op(&b.op_id, "fac::Int->Int", "b1", "c1");
        log.put(&c).unwrap();

        let walked = log.walk_back(&c.op_id, None).unwrap();
        let ids: Vec<_> = walked.iter().map(|r| r.op_id.as_str()).collect();
        assert_eq!(ids, vec![c.op_id.as_str(), b.op_id.as_str(), a.op_id.as_str()]);
    }

    #[test]
    fn list_all_dedups_across_loose_and_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        log.repack(0).unwrap();
        // Re-put the same op as a loose file (simulate an
        // interrupted repack). list_all should still report
        // exactly one record per op_id.
        log.put(&a).unwrap();

        let all = log.list_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].op_id, a.op_id);
    }

    #[test]
    fn walk_back_orders_ancestors_after_descendants() {
        // Build a small DAG with a merge:
        //
        //     a
        //    / \
        //   b   c
        //    \ /
        //     m  (merge with parents [b, c])
        //
        // The merge engine relies on the property that any ancestor of
        // X appears strictly after X in the walk_back output. Pin it.
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let a = add_op();
        log.put(&a).unwrap();
        let b = modify_op(&a.op_id, "fac::Int->Int", "abc123", "b1");
        log.put(&b).unwrap();
        let c = OperationRecord::new(
            Operation::new(
                OperationKind::ModifyBody {
                    sig_id: "double::Int->Int".into(),
                    from_stage_id: "ddd000".into(),
                    to_stage_id: "c1".into(),
                    from_budget: None,
                    to_budget: None,
                },
                [a.op_id.clone()],
            ),
            StageTransition::Replace {
                sig_id: "double::Int->Int".into(),
                from: "ddd000".into(),
                to: "c1".into(),
            },
        );
        log.put(&c).unwrap();
        let m = OperationRecord::new(
            Operation::new(
                OperationKind::Merge { resolved: 0 },
                [b.op_id.clone(), c.op_id.clone()],
            ),
            StageTransition::Merge { entries: BTreeMap::new() },
        );
        log.put(&m).unwrap();

        let walked = log.walk_back(&m.op_id, None).unwrap();
        let pos = |id: &str| walked.iter().position(|r| r.op_id == id).unwrap();
        let (m_pos, b_pos, c_pos, a_pos) =
            (pos(&m.op_id), pos(&b.op_id), pos(&c.op_id), pos(&a.op_id));
        // Each ancestor must appear strictly after its descendants.
        assert!(m_pos < b_pos, "merge before its parent b");
        assert!(m_pos < c_pos, "merge before its parent c");
        assert!(b_pos < a_pos, "b before its parent a");
        assert!(c_pos < a_pos, "c before its parent a");
    }
}
