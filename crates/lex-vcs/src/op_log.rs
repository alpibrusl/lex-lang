//! Persistence + DAG queries for the operation log.
//!
//! Layout: `<root>/ops/<op_id>.json` — one canonical-JSON file
//! per [`OperationRecord`]. Atomic writes via tempfile + rename.
//! Idempotent: writing an existing op_id is a no-op (content
//! addressing guarantees the bytes match).

use crate::operation::{OpId, OperationRecord};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Write};
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
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let rec: OperationRecord = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(rec))
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
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(id) = name.strip_suffix(".json") {
                if let Some(rec) = self.get(&id.to_string())? {
                    out.push(rec);
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
