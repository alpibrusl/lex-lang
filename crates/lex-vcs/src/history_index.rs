//! In-memory index of one head's op history (#971).
//!
//! [`OpLog::ops_since`] answers "ops reachable from `head` but not from
//! `base`" by walking the log on disk twice: once from `head` to genesis
//! and once from `base` to genesis, one file read + JSON parse per op.
//! That is the right shape for one question. It is the wrong shape for a
//! paged pull, which asks the same question once per page: a full pull of
//! an N-op history in pages of P ops re-reads the whole log N/P times.
//!
//! [`HistoryIndex`] does the disk walk from `head` once and keeps only the
//! op ids and parent edges (~200 bytes per op), not the records. Every later
//! question about that head — the delta from any `base`, the op at any
//! position — is answered in memory, and a page only touches disk for the
//! records it actually returns.
//!
//! The index is exact, not approximate: [`HistoryIndex::since`] returns
//! precisely `ops_since(head, base)` reversed (oldest-first), in the same
//! order. Op ids are content-addressed and the log is append-only, so the
//! history below a given head never changes and an index never goes stale
//! (short of `lex op gc` evicting ops, after which an evicted id simply
//! has no record to return).

use crate::op_log::OpLog;
use crate::operation::OpId;
use std::collections::{BTreeSet, HashMap};
use std::io;

/// Every op reachable from one head, in [`OpLog::walk_back`] order, with
/// its parent edges. See the module docs.
pub struct HistoryIndex {
    head: OpId,
    /// Newest-first: exactly the order `OpLog::walk_back(head, None)`
    /// returns.
    ids: Vec<OpId>,
    pos: HashMap<OpId, u32>,
    /// Parent edges as positions into `ids`, in CSR form: op `i`'s
    /// parents are `parents[parent_start[i]..parent_start[i + 1]]`. A
    /// parent with no record in the log has no position and no edge,
    /// matching `walk_back`, which cannot descend past a missing record.
    parent_start: Vec<u32>,
    parents: Vec<u32>,
}

impl HistoryIndex {
    /// Walk `head`'s history on disk once and index it. A `head` with no
    /// record in the log yields an empty index.
    pub fn build(log: &OpLog, head: &OpId) -> io::Result<Self> {
        let records = log.walk_back(head, None)?;
        let ids: Vec<OpId> = records.iter().map(|r| r.op_id.clone()).collect();
        let pos: HashMap<OpId, u32> =
            ids.iter().enumerate().map(|(i, id)| (id.clone(), i as u32)).collect();
        let mut parent_start = Vec::with_capacity(ids.len() + 1);
        let mut parents = Vec::new();
        for rec in &records {
            parent_start.push(parents.len() as u32);
            parents.extend(rec.op.parents.iter().filter_map(|p| pos.get(p).copied()));
        }
        parent_start.push(parents.len() as u32);
        Ok(Self { head: head.clone(), ids, pos, parent_start, parents })
    }

    pub fn head(&self) -> &OpId {
        &self.head
    }

    /// Number of ops reachable from the head.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The op id at a position returned by [`Self::since`].
    pub fn id(&self, i: u32) -> &OpId {
        &self.ids[i as usize]
    }

    /// Positions of the ops reachable from the head but not from `base`,
    /// **oldest-first** — exactly `OpLog::ops_since(head, base)` reversed,
    /// element for element.
    ///
    /// When `base` is in this history its ancestry is computed in memory.
    /// Otherwise (a `base` on a history the head doesn't contain, or one
    /// the log has never seen) the ancestry is walked on disk, as
    /// `ops_since` does; ops outside the head's history can't be in the
    /// result either way, so only the membership test differs.
    pub fn since(&self, log: &OpLog, base: Option<&OpId>) -> io::Result<Vec<u32>> {
        let mut excluded = vec![false; self.ids.len()];
        match base {
            None => {}
            Some(b) => match self.pos.get(b) {
                Some(&start) => {
                    let mut stack = vec![start];
                    excluded[start as usize] = true;
                    while let Some(i) = stack.pop() {
                        let i = i as usize;
                        let (lo, hi) = (self.parent_start[i] as usize, self.parent_start[i + 1] as usize);
                        for &p in &self.parents[lo..hi] {
                            if !excluded[p as usize] {
                                excluded[p as usize] = true;
                                stack.push(p);
                            }
                        }
                    }
                }
                None => {
                    let anc: BTreeSet<OpId> =
                        log.walk_back(b, None)?.into_iter().map(|r| r.op_id).collect();
                    for (i, id) in self.ids.iter().enumerate() {
                        excluded[i] = anc.contains(id);
                    }
                }
            },
        }
        Ok((0..self.ids.len() as u32).rev().filter(|&i| !excluded[i as usize]).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{Operation, OperationKind, OperationRecord, StageTransition};
    use std::collections::BTreeMap;

    fn rec(parents: &[&OpId], tag: usize) -> OperationRecord {
        let kind = if parents.len() > 1 {
            OperationKind::Merge { resolved: tag }
        } else {
            OperationKind::AddFunction {
                sig_id: format!("s{tag}"),
                stage_id: format!("t{tag}"),
                effects: BTreeSet::new(),
                budget_cost: None,
                in_file: None,
            }
        };
        let produces = if parents.len() > 1 {
            StageTransition::Merge { entries: BTreeMap::new() }
        } else {
            StageTransition::Create { sig_id: format!("s{tag}"), stage_id: format!("t{tag}") }
        };
        OperationRecord::new(Operation::new(kind, parents.iter().map(|p| (*p).clone())), produces)
    }

    /// A deterministic pseudo-random DAG: several roots, branches off
    /// arbitrary earlier ops, merges of two and three parents.
    fn random_dag(log: &OpLog, n: usize, seed: u64) -> Vec<OpId> {
        let mut state = seed;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut ids: Vec<OpId> = Vec::new();
        for i in 0..n {
            let r = next();
            let parents: Vec<&OpId> = if ids.is_empty() || r % 17 == 0 {
                vec![]
            } else {
                let k = match r % 7 { 0 => 2, 1 => 3, _ => 1 };
                (0..k).map(|_| &ids[(next() as usize) % ids.len()]).collect()
            };
            let rec = rec(&parents, i);
            log.put(&rec).unwrap();
            ids.push(rec.op_id);
        }
        ids
    }

    fn oracle(log: &OpLog, head: &OpId, base: Option<&OpId>) -> Vec<OpId> {
        let mut v: Vec<OpId> = log.ops_since(head, base).unwrap().into_iter().map(|r| r.op_id).collect();
        v.reverse();
        v
    }

    #[test]
    fn since_matches_ops_since_on_random_dags() {
        for seed in [1u64, 7, 42, 1234, 99991] {
            let tmp = tempfile::tempdir().unwrap();
            let log = OpLog::open(tmp.path()).unwrap();
            let ids = random_dag(&log, 120, seed);
            let ghost: OpId = "0".repeat(64);
            for head in ids.iter().step_by(13).chain(ids.last()) {
                let idx = HistoryIndex::build(&log, head).unwrap();
                assert_eq!(idx.len(), log.walk_back(head, None).unwrap().len());
                let bases = std::iter::once(None)
                    .chain(ids.iter().step_by(7).map(Some))
                    .chain(std::iter::once(Some(&ghost)));
                for base in bases {
                    let got: Vec<OpId> =
                        idx.since(&log, base).unwrap().into_iter().map(|i| idx.id(i).clone()).collect();
                    assert_eq!(got, oracle(&log, head, base), "seed={seed} head={head} base={base:?}");
                }
            }
        }
    }

    #[test]
    fn unknown_head_is_an_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let idx = HistoryIndex::build(&log, &"f".repeat(64)).unwrap();
        assert!(idx.is_empty());
        assert!(idx.since(&log, None).unwrap().is_empty());
    }
}
