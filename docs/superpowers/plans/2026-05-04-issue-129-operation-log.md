# Issue #129 Operation Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the operation log the durable source of truth in `lex-store`. Branches store an `OpId` head; the `SigId → StageId` map every consumer reads is computed by replaying the op DAG. `lex publish` produces typed operations; `lex blame` walks the op graph; `lex op show` and `lex op log` ship as new subcommands.

**Architecture:** Two new modules in `lex-vcs` (`op_log` for persistence + DAG queries, `apply` for the apply gate) plus `diff_to_ops` and `merge`. `lex-store` rewrites its branch model to a single `head_op` field, computes `branch_head` on demand, and exposes a single `apply_operation` advance path. `lex-cli` and `lex-api` route their write paths through it. No on-disk migration: previous-format store directories stop being readable.

**Tech Stack:** Rust 2021, `serde` + `serde_json`, `sha2`, `thiserror`, `indexmap`, `tempfile` (already a dev-dep). New `lex-vcs` dep on `lex-ast` (only `diff_to_ops` uses it).

**Spec:** [`docs/superpowers/specs/2026-05-04-issue-129-operation-log-design.md`](../specs/2026-05-04-issue-129-operation-log-design.md).

---

## File Structure

| Path | Status | Responsibility |
|---|---|---|
| `crates/lex-vcs/src/operation.rs` | modify | Add `Merge` to `OperationKind`, add `Merge` to `StageTransition`. |
| `crates/lex-vcs/src/op_log.rs` | create | Persistence + DAG queries (`put`, `get`, `walk_back`, `walk_forward`, `lca`, `ops_since`). |
| `crates/lex-vcs/src/apply.rs` | create | The `apply` function: parent-consistency check + persist op record. |
| `crates/lex-vcs/src/diff_to_ops.rs` | create | Convert an `ast-diff` `DiffReport` + import set deltas into `Vec<OperationKind>`. |
| `crates/lex-vcs/src/merge.rs` | create | Op-DAG three-way merge. |
| `crates/lex-vcs/src/lib.rs` | modify | Export new modules. |
| `crates/lex-vcs/Cargo.toml` | modify | Add `lex-ast` dep + `tempfile` dev-dep. |
| `crates/lex-store/src/branches.rs` | modify | Drop `head`/`fork_base`; add `head_op`; rewrite `branch_head` as computed view; rewrite `merge`/`commit_merge` over the op DAG; remove `set_branch_head_entry`. |
| `crates/lex-store/src/store.rs` | modify | Add `apply_operation` and `op_log` accessor. |
| `crates/lex-store/src/lib.rs` | modify | Re-export `OpId` and friends from `lex-vcs`; drop now-unused exports. |
| `crates/lex-store/Cargo.toml` | modify | Add `lex-vcs` dep. |
| `crates/lex-store/tests/branches.rs` | modify | Rewrite tests to drive merges via `apply_operation` (no more `set_branch_head_entry`). |
| `crates/lex-cli/src/diff.rs` | modify | Make `DiffReport` and its sub-types `pub` so `lex-vcs::diff_to_ops` can take them as input. |
| `crates/lex-cli/src/main.rs` | modify | Wire new `op` subcommand; refactor `cmd_publish` and `cmd_blame`. |
| `crates/lex-cli/src/op.rs` | create | `cmd_op`, `cmd_op_show`, `cmd_op_log`. |
| `crates/lex-cli/tests/op.rs` | create | Integration tests for `lex op show` / `lex op log`. |
| `crates/lex-cli/tests/publish.rs` | create | Integration tests for the refactored `lex publish`. |
| `crates/lex-api/src/handlers.rs` | modify | Refactor `publish_handler` and `patch_handler` to route through `Store::apply_operation`. |
| `README.md` | modify | Update tier-1 status row; add tier-2 row noting #129 done, #130 next. |

---

## Task 1: Extend the Operation enums for merges

**Files:**
- Modify: `crates/lex-vcs/src/operation.rs`

This is groundwork: the merge engine in Task 6 needs `OperationKind::Merge` and `StageTransition::Merge` to exist, but adding them is mechanical and independent of the rest. Doing it first unblocks parallel work.

- [ ] **Step 1: Add the failing tests for the new variants**

In `crates/lex-vcs/src/operation.rs`, inside `mod tests`, add:

```rust
#[test]
fn merge_kind_round_trips() {
    let op = Operation::new(
        OperationKind::Merge { resolved: 3 },
        ["op-a".into(), "op-b".into()],
    );
    let json = serde_json::to_string(&op).expect("ser");
    let back: Operation = serde_json::from_str(&json).expect("de");
    assert_eq!(op, back);
    assert_eq!(op.op_id(), back.op_id());
}

#[test]
fn merge_stage_transition_round_trips() {
    let mut entries = std::collections::BTreeMap::new();
    entries.insert("sig-a".to_string(), Some("stage-a".to_string()));
    entries.insert("sig-b".to_string(), None); // removed by merge
    let t = StageTransition::Merge { entries };
    let json = serde_json::to_string(&t).expect("ser");
    let back: StageTransition = serde_json::from_str(&json).expect("de");
    assert_eq!(t, back);
}

#[test]
fn merge_resolved_count_changes_op_id() {
    // Two merges with the same parents but different resolved counts
    // must hash differently — keeps structurally distinct merges from
    // colliding on op_id.
    let parents: Vec<OpId> = vec!["op-a".into(), "op-b".into()];
    let one = Operation::new(OperationKind::Merge { resolved: 1 }, parents.clone());
    let two = Operation::new(OperationKind::Merge { resolved: 2 }, parents);
    assert_ne!(one.op_id(), two.op_id());
}

#[test]
fn existing_add_function_op_id_is_unchanged_after_merge_added() {
    // Sanity: adding a new variant to the serde-tagged enum must not
    // perturb the canonical bytes (or therefore op_id) of existing
    // variants. The golden hash test below is also a guard, but this
    // one fails with a clearer message if tag rendering shifts.
    let op = Operation::new(add_factorial(), []);
    assert_eq!(
        op.op_id(),
        "f112990d31ef2a63f3e5ca5680637ed36a54bc7e8230510ae0c0e93fcb39d104"
    );
}
```

- [ ] **Step 2: Run tests to confirm they fail**

Run: `cargo test -p lex-vcs operation::tests::merge`
Expected: FAIL — `Merge` variant doesn't exist yet.

- [ ] **Step 3: Add the variants**

In `OperationKind`, after `ModifyType`:

```rust
/// Merge of two branch heads. Carries only an informational count
/// of resolved sigs so two structurally identical merges of
/// different sizes don't collide on op_id; the per-sig deltas live
/// in `OperationRecord::produces` (`StageTransition::Merge`).
Merge {
    resolved: usize,
},
```

In `StageTransition`, after `ImportOnly`:

```rust
/// Merge op result. `entries` lists only the sigs whose head
/// changed relative to the merge op's first parent (`dst_head`):
/// `Some(stage_id)` sets the head; `None` removes the sig.
/// Sigs unaffected by the merge are not listed.
Merge {
    entries: std::collections::BTreeMap<SigId, Option<StageId>>,
},
```

- [ ] **Step 4: Run all `lex-vcs` tests**

Run: `cargo test -p lex-vcs`
Expected: PASS — all old tests (including the golden hash) still pass; new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/lex-vcs/src/operation.rs
git commit -m "$(cat <<'EOF'
feat(lex-vcs): add Merge variants to OperationKind and StageTransition

Additive — existing variants' canonical bytes are unchanged, so
existing op_ids and the golden hash stay stable.

Refs #129.
EOF
)"
```

---

## Task 2: `OpLog` persistence and DAG queries

**Files:**
- Create: `crates/lex-vcs/src/op_log.rs`
- Modify: `crates/lex-vcs/src/lib.rs`
- Modify: `crates/lex-vcs/Cargo.toml`

`OpLog` owns `<root>/ops/`. Tests use `tempfile`.

- [ ] **Step 1: Add `tempfile` to lex-vcs dev-deps**

In `crates/lex-vcs/Cargo.toml`, under `[dev-dependencies]`:

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write failing tests for `put` / `get`**

Create `crates/lex-vcs/src/op_log.rs`:

```rust
//! Persistence + DAG queries for the operation log.
//!
//! Layout: `<root>/ops/<op_id>.json` — one canonical-JSON file
//! per [`OperationRecord`]. Atomic writes via tempfile + rename.
//! Idempotent: writing an existing op_id is a no-op (content
//! addressing guarantees the bytes match).

use crate::operation::{OpId, OperationRecord};
use std::collections::BTreeSet;
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

    pub fn put(&self, rec: &OperationRecord) -> io::Result<()> {
        let path = self.path(&rec.op_id);
        if path.exists() { return Ok(()); }
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
        if !path.exists() { return Ok(None); }
        let bytes = fs::read(&path)?;
        let rec: OperationRecord = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(rec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{Operation, OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn add_op() -> OperationRecord {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac::Int->Int".into(),
                stage_id: "abc123".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        OperationRecord::new(op, StageTransition::Create {
            sig_id: "fac::Int->Int".into(),
            stage_id: "abc123".into(),
        })
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
}
```

In `crates/lex-vcs/src/lib.rs`, add the module:

```rust
mod canonical;
mod op_log;
mod operation;

pub use op_log::OpLog;
pub use operation::{
    EffectSet, ModuleRef, OpId, Operation, OperationRecord, OperationKind, SigId, StageId,
    StageTransition,
};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p lex-vcs op_log`
Expected: PASS — three tests green.

- [ ] **Step 4: Add failing tests for DAG queries**

Append to `op_log.rs` test module:

```rust
fn modify_op(parent: &OpId, sig: &str, from: &str, to: &str) -> OperationRecord {
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.into(),
            from_stage_id: from.into(),
            to_stage_id: to.into(),
        },
        [parent.clone()],
    );
    OperationRecord::new(op, StageTransition::Replace {
        sig_id: sig.into(),
        from: from.into(),
        to: to.into(),
    })
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
    assert_eq!(ids, vec![c.op_id.as_str(), b.op_id.as_str(), a.op_id.as_str()]);
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
    let left  = modify_op(&root.op_id, "fac::Int->Int", "abc123", "left1");
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
        Operation::new(OperationKind::AddFunction {
            sig_id: "double::Int->Int".into(),
            stage_id: "ddd111".into(),
            effects: BTreeSet::new(),
        }, []),
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

    let since: Vec<_> = log.ops_since(&c.op_id, Some(&a.op_id)).unwrap()
        .into_iter().map(|r| r.op_id).collect();
    assert_eq!(since.len(), 2);
    assert!(since.contains(&b.op_id));
    assert!(since.contains(&c.op_id));
    assert!(!since.contains(&a.op_id));
}
```

- [ ] **Step 5: Run them — confirm failure**

Run: `cargo test -p lex-vcs op_log`
Expected: FAIL — `walk_back` / `walk_forward` / `lca` / `ops_since` don't exist.

- [ ] **Step 6: Implement the queries**

Append to `OpLog` impl:

```rust
    /// Walk parents transitively. Newest-first, BFS, dedup'd by op_id.
    /// Stops at parentless ops or after `limit` records.
    pub fn walk_back(
        &self,
        head: &OpId,
        limit: Option<usize>,
    ) -> io::Result<Vec<OperationRecord>> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut frontier = vec![head.clone()];
        while let Some(id) = frontier.pop() {
            if !seen.insert(id.clone()) { continue; }
            if let Some(rec) = self.get(&id)? {
                // Push parents before recording so traversal order is
                // a stable BFS-by-discovery: children-first, then their
                // parents, parents of those, etc.
                for p in &rec.op.parents {
                    if !seen.contains(p) {
                        frontier.insert(0, p.clone());
                    }
                }
                out.push(rec);
                if let Some(n) = limit { if out.len() >= n { break; } }
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
        if let Some(n) = limit { all.truncate(n); }
        Ok(all)
    }

    /// Lowest common ancestor of two op_ids in the DAG. None if no
    /// shared ancestor exists.
    pub fn lca(&self, a: &OpId, b: &OpId) -> io::Result<Option<OpId>> {
        let a_anc: BTreeSet<OpId> = self.walk_back(a, None)?
            .into_iter().map(|r| r.op_id).collect();
        // Walk b's ancestors in newest-first order; first hit is the LCA
        // (newest-first guarantees the deepest, i.e. lowest, ancestor
        // common to both).
        for rec in self.walk_back(b, None)? {
            if a_anc.contains(&rec.op_id) {
                return Ok(Some(rec.op_id));
            }
        }
        Ok(None)
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
            Some(b) => self.walk_back(b, None)?
                .into_iter().map(|r| r.op_id).collect(),
            None => BTreeSet::new(),
        };
        Ok(self.walk_back(head, None)?
            .into_iter()
            .filter(|r| !exclude.contains(&r.op_id))
            .collect())
    }
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p lex-vcs op_log`
Expected: PASS — all 8 tests green.

- [ ] **Step 8: Commit**

```bash
git add crates/lex-vcs/src/op_log.rs crates/lex-vcs/src/lib.rs crates/lex-vcs/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(lex-vcs): OpLog with persistence + DAG queries

put/get with tempfile-rename atomicity, idempotent on existing
op_ids. walk_back/walk_forward + lca + ops_since for traversal.

Refs #129.
EOF
)"
```

---

## Task 3: `apply` function (parent-consistency check + persist)

**Files:**
- Create: `crates/lex-vcs/src/apply.rs`
- Modify: `crates/lex-vcs/src/lib.rs`

The apply call validates an op's parents against a known head, persists it via `OpLog::put`, and returns the new head.

- [ ] **Step 1: Write the failing tests**

Create `crates/lex-vcs/src/apply.rs`:

```rust
//! The apply gate. Validates an operation's parents against a known
//! branch head, then persists it via [`OpLog`]. Issue #129 keeps this
//! narrow: no type checking, no effect verification — those are #130.

use crate::op_log::OpLog;
use crate::operation::{OpId, Operation, OperationRecord, StageTransition};
use std::io;

#[derive(Debug)]
pub struct NewHead {
    pub op_id: OpId,
    pub record: OperationRecord,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("stale parent: branch head is {expected:?} but op's parents are {op_parents:?}")]
    StaleParent {
        expected: Option<OpId>,
        op_parents: Vec<OpId>,
    },
    #[error("merge op references unknown second parent {0}")]
    UnknownMergeParent(OpId),
    #[error(transparent)]
    Persist(#[from] io::Error),
}

/// Apply an operation against a branch head and persist it.
///
/// Validates parents:
/// - If `op.parents.is_empty()`: `head_op` must be `None` (genesis op
///   on an empty branch).
/// - If `op.parents.len() == 1`: that parent must equal `head_op`.
/// - If `op.parents.len() == 2`: one parent must equal `head_op`, and
///   the other must already exist in the log (a merge op's
///   second-parent ancestry must be reachable).
/// - All other arities are rejected as `StaleParent`.
pub fn apply(
    op_log: &OpLog,
    head_op: Option<&OpId>,
    op: Operation,
    transition: StageTransition,
) -> Result<NewHead, ApplyError> {
    match (op.parents.len(), head_op) {
        (0, None) => {}
        (1, Some(h)) if op.parents[0] == *h => {}
        (2, Some(h)) => {
            if op.parents[0] != *h && op.parents[1] != *h {
                return Err(ApplyError::StaleParent {
                    expected: head_op.cloned(),
                    op_parents: op.parents.clone(),
                });
            }
            // The non-head parent must exist in the log.
            let other = if op.parents[0] == *h { &op.parents[1] } else { &op.parents[0] };
            if op_log.get(other)?.is_none() {
                return Err(ApplyError::UnknownMergeParent(other.clone()));
            }
        }
        _ => {
            return Err(ApplyError::StaleParent {
                expected: head_op.cloned(),
                op_parents: op.parents.clone(),
            });
        }
    }

    let record = OperationRecord::new(op, transition);
    op_log.put(&record)?;
    Ok(NewHead { op_id: record.op_id.clone(), record })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn add_fac() -> (Operation, StageTransition) {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: "fac".into(),
                stage_id: "s1".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        let t = StageTransition::Create {
            sig_id: "fac".into(),
            stage_id: "s1".into(),
        };
        (op, t)
    }

    #[test]
    fn parentless_op_against_empty_head_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op, t) = add_fac();
        let head = apply(&log, None, op, t).unwrap();
        assert!(log.get(&head.op_id).unwrap().is_some());
    }

    #[test]
    fn parentless_op_against_non_empty_head_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op1, t1) = add_fac();
        let head1 = apply(&log, None, op1, t1).unwrap();
        let (op2, t2) = add_fac(); // parentless again
        let err = apply(&log, Some(&head1.op_id), op2, t2).unwrap_err();
        assert!(matches!(err, ApplyError::StaleParent { .. }));
    }

    #[test]
    fn single_parent_matching_head_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op1, t1) = add_fac();
        let head1 = apply(&log, None, op1, t1).unwrap();
        let modify = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fac".into(),
                from_stage_id: "s1".into(),
                to_stage_id: "s2".into(),
            },
            [head1.op_id.clone()],
        );
        let t = StageTransition::Replace {
            sig_id: "fac".into(),
            from: "s1".into(),
            to: "s2".into(),
        };
        let head2 = apply(&log, Some(&head1.op_id), modify, t).unwrap();
        assert_ne!(head2.op_id, head1.op_id);
    }

    #[test]
    fn single_parent_not_matching_head_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op1, t1) = add_fac();
        let head1 = apply(&log, None, op1, t1).unwrap();
        // op claims a different parent than head.
        let bogus = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fac".into(),
                from_stage_id: "s1".into(),
                to_stage_id: "s2".into(),
            },
            ["someone-else".into()],
        );
        let t = StageTransition::Replace {
            sig_id: "fac".into(),
            from: "s1".into(),
            to: "s2".into(),
        };
        let err = apply(&log, Some(&head1.op_id), bogus, t).unwrap_err();
        assert!(matches!(err, ApplyError::StaleParent { .. }));
    }

    #[test]
    fn merge_op_with_known_second_parent_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op_a, t_a) = add_fac();
        let head_a = apply(&log, None, op_a, t_a).unwrap();
        let other = Operation::new(
            OperationKind::AddFunction {
                sig_id: "double".into(),
                stage_id: "d1".into(),
                effects: BTreeSet::new(),
            },
            [],
        );
        let head_b = apply(&log, None, other, StageTransition::Create {
            sig_id: "double".into(), stage_id: "d1".into(),
        }).unwrap();
        // Merge op: parents = [head_a, head_b].
        let merge = Operation::new(
            OperationKind::Merge { resolved: 1 },
            [head_a.op_id.clone(), head_b.op_id.clone()],
        );
        let t = StageTransition::Merge {
            entries: std::iter::once(("double".to_string(), Some("d1".to_string())))
                .collect(),
        };
        let merged = apply(&log, Some(&head_a.op_id), merge, t).unwrap();
        assert!(log.get(&merged.op_id).unwrap().is_some());
    }

    #[test]
    fn merge_op_with_unknown_second_parent_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let (op_a, t_a) = add_fac();
        let head_a = apply(&log, None, op_a, t_a).unwrap();
        let merge = Operation::new(
            OperationKind::Merge { resolved: 0 },
            [head_a.op_id.clone(), "ghost".into()],
        );
        let t = StageTransition::Merge { entries: Default::default() };
        let err = apply(&log, Some(&head_a.op_id), merge, t).unwrap_err();
        assert!(matches!(err, ApplyError::UnknownMergeParent(_)));
    }
}
```

In `crates/lex-vcs/src/lib.rs`, add the module + exports:

```rust
mod apply;
mod canonical;
mod op_log;
mod operation;

pub use apply::{apply, ApplyError, NewHead};
pub use op_log::OpLog;
pub use operation::{
    EffectSet, ModuleRef, OpId, Operation, OperationRecord, OperationKind, SigId, StageId,
    StageTransition,
};
```

- [ ] **Step 2: Run tests — confirm they fail**

Run: `cargo test -p lex-vcs apply`
Expected: FAIL (compile error: `apply` doesn't exist).

- [ ] **Step 3: Confirm tests pass after the impl**

(The impl is included in step 1's file content above.) Re-run:

Run: `cargo test -p lex-vcs apply`
Expected: PASS — six tests green.

- [ ] **Step 4: Commit**

```bash
git add crates/lex-vcs/src/apply.rs crates/lex-vcs/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(lex-vcs): apply gate with parent-consistency check

Validates op.parents against the supplied head_op (genesis,
single-parent, or merge), persists via OpLog::put, returns a
NewHead. Type checking is deferred to #130.

Refs #129.
EOF
)"
```

---

## Task 4: `lex-store` schema rewrite — head_op + computed branch_head

**Files:**
- Modify: `crates/lex-store/Cargo.toml`
- Modify: `crates/lex-store/src/branches.rs`
- Modify: `crates/lex-store/src/store.rs`
- Modify: `crates/lex-store/src/lib.rs`

This is the destructive change. The `Branch` struct loses `head` and `fork_base`; `branch_head` becomes computed. `set_branch_head_entry` is removed. `merge` and `commit_merge` are *temporarily stubbed* in this task — they get rewritten on top of the op-DAG engine in Task 7. Tests in `tests/branches.rs` are deleted in this task and rewritten in Task 7 over the new merge engine.

- [ ] **Step 1: Add `lex-vcs` dep**

In `crates/lex-store/Cargo.toml`, under `[dependencies]`:

```toml
lex-vcs = { path = "../lex-vcs" }
```

- [ ] **Step 2: Replace `Branch` and rewrite `branch_head`**

In `crates/lex-store/src/branches.rs`, replace the entire file with:

```rust
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
        let head_op = self.get_branch(from)?.and_then(|b| b.head_op).clone();
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
    let bytes = serde_json::to_vec_pretty(b)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// `merge` and `commit_merge` are stubbed pending the op-DAG engine
// in Task 7 of the #129 plan.
impl Store {
    pub fn merge(&self, _src: &str, _dst: &str) -> Result<MergeReport, StoreError> {
        Err(StoreError::InvalidTransition(
            "merge: pending op-DAG engine (#129 task 7)".into()))
    }

    pub fn commit_merge(&self, _dst: &str, _report: &MergeReport) -> Result<(), StoreError> {
        Err(StoreError::InvalidTransition(
            "commit_merge: pending op-DAG engine (#129 task 7)".into()))
    }
}
```

- [ ] **Step 3: Update `lib.rs` exports**

In `crates/lex-store/src/lib.rs`:

```rust
mod store;
mod model;
mod branches;

pub use lex_vcs::{OpId, Operation, OperationKind, OperationRecord, StageTransition};
pub use model::{Lifecycle, Metadata, Spec, StageStatus, Test, Transition};
pub use store::{StageHistoryEntry, Store, StoreError};
pub use branches::{
    Branch, MergeConflict, MergeEntry, MergeRecord, MergeReport, MergeSummary,
    DEFAULT_BRANCH,
};
```

- [ ] **Step 4: Delete the now-obsolete tier-1 branch tests**

The existing `crates/lex-store/tests/branches.rs` exercises `set_branch_head_entry` (removed) and the old merge engine (stubbed). Replace it with a placeholder that compiles but covers nothing yet — Task 7 fills it back in:

```rust
//! Branch tests for the op-DAG model. Populated in #129 task 7
//! (op-DAG merge engine).

#[test]
fn placeholder() {
    // Intentionally empty pending the op-DAG merge engine in task 7.
}
```

- [ ] **Step 5: Verify the workspace still compiles**

Run: `cargo build --workspace`
Expected: errors in callers of `set_branch_head_entry`. There are two: `crates/lex-store/tests/branches.rs` (already replaced above) and any internal use. Grep first to be sure.

```bash
grep -rn "set_branch_head_entry" crates/
```

If any other call site shows up (other than the now-replaced test file), remove the call — it should not be reachable from production code in this PR's scope.

- [ ] **Step 6: Run lex-store tests**

Run: `cargo test -p lex-store`
Expected: PASS — `m6.rs` tests still green (`store.publish` etc. unchanged); `branches.rs` placeholder passes.

- [ ] **Step 7: Commit**

```bash
git add crates/lex-store/Cargo.toml crates/lex-store/src/branches.rs \
        crates/lex-store/src/lib.rs crates/lex-store/tests/branches.rs
git commit -m "$(cat <<'EOF'
refactor(lex-store): branches store head_op; branch_head computed

Drop head/fork_base fields. branch_head walks the op log from
head_op and replays transitions. set_branch_head_entry removed —
apply_operation (next task) is the single advance path.

merge / commit_merge stubbed; rewritten in task 7 of #129.

Refs #129.
EOF
)"
```

---

## Task 5: `Store::apply_operation` glue

**Files:**
- Modify: `crates/lex-store/src/store.rs`

The call CLI and API both go through.

- [ ] **Step 1: Write the failing test**

Create `crates/lex-store/tests/apply_operation.rs`:

```rust
//! `Store::apply_operation` — the only way to advance a branch
//! head's op (post-#129).

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

#[test]
fn apply_operation_advances_head_on_main() {
    let (s, _tmp) = fresh();
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let t = StageTransition::Create {
        sig_id: "fac".into(),
        stage_id: "stg-1".into(),
    };
    let op_id = s.apply_operation(DEFAULT_BRANCH, op, t).unwrap();
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-1".to_string()));

    // The branch file now exists with this head_op.
    let b = s.get_branch(DEFAULT_BRANCH).unwrap().unwrap();
    assert_eq!(b.head_op.as_deref(), Some(op_id.as_str()));
}

#[test]
fn apply_operation_chains_against_existing_head() {
    let (s, _tmp) = fresh();
    let op1 = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    let op_id1 = s.apply_operation(DEFAULT_BRANCH, op1, StageTransition::Create {
        sig_id: "fac".into(), stage_id: "stg-1".into(),
    }).unwrap();
    let op2 = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
        },
        [op_id1.clone()],
    );
    let op_id2 = s.apply_operation(DEFAULT_BRANCH, op2, StageTransition::Replace {
        sig_id: "fac".into(), from: "stg-1".into(), to: "stg-2".into(),
    }).unwrap();
    assert_ne!(op_id1, op_id2);
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-2".to_string()));
}

#[test]
fn apply_operation_with_stale_parent_errors() {
    let (s, _tmp) = fresh();
    let op1 = Operation::new(
        OperationKind::AddFunction {
            sig_id: "fac".into(),
            stage_id: "stg-1".into(),
            effects: BTreeSet::new(),
        },
        [],
    );
    s.apply_operation(DEFAULT_BRANCH, op1, StageTransition::Create {
        sig_id: "fac".into(), stage_id: "stg-1".into(),
    }).unwrap();
    // Op claims a different parent than the current head.
    let bogus = Operation::new(
        OperationKind::ModifyBody {
            sig_id: "fac".into(),
            from_stage_id: "stg-1".into(),
            to_stage_id: "stg-2".into(),
        },
        ["someone-else".into()],
    );
    let err = s.apply_operation(DEFAULT_BRANCH, bogus, StageTransition::Replace {
        sig_id: "fac".into(), from: "stg-1".into(), to: "stg-2".into(),
    });
    assert!(err.is_err(), "expected stale-parent rejection");
    // Head is unchanged.
    let head = s.branch_head(DEFAULT_BRANCH).unwrap();
    assert_eq!(head.get("fac"), Some(&"stg-1".to_string()));
}
```

- [ ] **Step 2: Run — confirm failure**

Run: `cargo test -p lex-store apply_operation`
Expected: FAIL (compile error: `Store::apply_operation` doesn't exist).

- [ ] **Step 3: Implement `apply_operation`**

In `crates/lex-store/src/store.rs`, add at the bottom of `impl Store`:

```rust
    /// Apply an operation to a branch and advance its head_op.
    ///
    /// The single advance path. Validates parents via lex_vcs::apply,
    /// persists the operation, then atomically advances the branch
    /// file's head_op.
    pub fn apply_operation(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let head_op = self.get_branch(branch)?.and_then(|b| b.head_op);
        let new_head = lex_vcs::apply(&log, head_op.as_ref(), op, transition)
            .map_err(|e| match e {
                lex_vcs::ApplyError::Persist(io) => StoreError::Io(io),
                other => StoreError::InvalidTransition(other.to_string()),
            })?;
        self.set_branch_head_op(branch, new_head.op_id.clone())?;
        Ok(new_head.op_id)
    }
```

Add `lex_vcs` import at the top of `store.rs`:

```rust
use lex_vcs;
```

(or use fully-qualified paths inline as shown above; either works).

- [ ] **Step 4: Run tests**

Run: `cargo test -p lex-store apply_operation`
Expected: PASS — three tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/lex-store/src/store.rs crates/lex-store/tests/apply_operation.rs
git commit -m "$(cat <<'EOF'
feat(lex-store): apply_operation as single branch-advance path

Routes ops through lex_vcs::apply, persists via OpLog, advances
branch.head_op atomically. Stale-parent errors leave head
unchanged.

Refs #129.
EOF
)"
```

---

## Task 6: `diff_to_ops` — turn an ast-diff into a typed op sequence

**Files:**
- Modify: `crates/lex-cli/src/diff.rs` (export the diff types)
- Modify: `crates/lex-vcs/Cargo.toml` (add `lex-ast` dep)
- Create: `crates/lex-vcs/src/diff_to_ops.rs`
- Modify: `crates/lex-vcs/src/lib.rs`

The CLI's `lex diff` already produces a `DiffReport` with all the data we need. We make those types `pub` so `lex-vcs` can consume them, and write a function that maps `DiffReport` + import set deltas + a previous head map onto a `Vec<OperationKind>`.

- [ ] **Step 1: Make the diff types `pub`**

In `crates/lex-cli/src/diff.rs`, add `pub` to the `DiffReport`-related structs:

```rust
#[derive(Serialize)]
pub struct AddRemove {
    pub name: String,
    pub signature: String,
}

#[derive(Serialize)]
pub struct Renamed {
    pub from: String,
    pub to: String,
    pub signature: String,
}

#[derive(Serialize)]
pub struct Modified {
    pub name: String,
    pub signature_before: String,
    pub signature_after: String,
    pub signature_changed: bool,
    pub effect_changes: EffectChanges,
    pub body_patches: Vec<BodyPatch>,
}

#[derive(Serialize, Default)]
pub struct EffectChanges {
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct BodyPatch {
    pub op: String,
    pub node_path: String,
    pub from_kind: String,
    pub to_kind: String,
}

#[derive(Serialize, Default)]
pub struct DiffReport {
    pub added: Vec<AddRemove>,
    pub removed: Vec<AddRemove>,
    pub renamed: Vec<Renamed>,
    pub modified: Vec<Modified>,
}
```

We will *not* depend on `lex-cli` from `lex-vcs` (cyclic). Instead we mirror these as plain types inside `lex-vcs`, accept any value matching the structure via a small adapter, *or* extract the `DiffReport` types into a small shared module. The simplest and cleanest fix:

- Move the `DiffReport`/`Modified`/`AddRemove`/`Renamed`/`EffectChanges`/`BodyPatch` struct definitions into a new file `crates/lex-vcs/src/diff_report.rs` (these are pure data types).
- In `lex-cli/src/diff.rs`, replace the local definitions with `pub use lex_vcs::diff_report::*;`.

Apply that move. After the move, `lex-cli/src/diff.rs` should `use lex_vcs::diff_report::DiffReport;` (and the other types) and the type definitions inside `diff.rs` are deleted.

- [ ] **Step 2: Create `diff_report.rs` and re-export it**

`crates/lex-vcs/src/diff_report.rs`:

```rust
//! Plain-data shape of the `lex ast-diff` output. Lives in lex-vcs
//! so both the CLI (which produces it) and `diff_to_ops` (which
//! consumes it) can share types without a cyclic dep.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AddRemove {
    pub name: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Renamed {
    pub from: String,
    pub to: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Modified {
    pub name: String,
    pub signature_before: String,
    pub signature_after: String,
    pub signature_changed: bool,
    pub effect_changes: EffectChanges,
    pub body_patches: Vec<BodyPatch>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EffectChanges {
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BodyPatch {
    pub op: String,
    pub node_path: String,
    pub from_kind: String,
    pub to_kind: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiffReport {
    pub added: Vec<AddRemove>,
    pub removed: Vec<AddRemove>,
    pub renamed: Vec<Renamed>,
    pub modified: Vec<Modified>,
}
```

Update `crates/lex-vcs/src/lib.rs`:

```rust
pub mod diff_report;
mod apply;
mod canonical;
mod op_log;
mod operation;
```

In `crates/lex-cli/src/diff.rs`, delete the local struct definitions and `use lex_vcs::diff_report::{...};` instead. Verify CLI still compiles.

- [ ] **Step 3: Add `lex-ast` dep to lex-vcs**

In `crates/lex-vcs/Cargo.toml`:

```toml
[dependencies]
lex-ast = { path = "../lex-ast" }
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
indexmap.workspace = true
```

- [ ] **Step 4: Write failing tests for `diff_to_ops`**

Create `crates/lex-vcs/src/diff_to_ops.rs`:

```rust
//! Convert a `DiffReport` (+ import set deltas + previous head)
//! into a sequence of typed operations.

use crate::diff_report::DiffReport;
use crate::operation::{EffectSet, ModuleRef, OperationKind, SigId, StageId};
use lex_ast::{sig_id, stage_id, Effect, FnDecl, Stage, TypeExpr};
use std::collections::{BTreeMap, BTreeSet};

/// Per-file map of imported `ModuleRef`s. The current import set is
/// derived from the op log on the current branch (option (ii) in
/// the design — no `imports/<file>.json` sidecar).
pub type ImportMap = BTreeMap<String, BTreeSet<ModuleRef>>;

#[derive(Debug)]
pub struct DiffInputs<'a> {
    pub old_head: &'a BTreeMap<SigId, StageId>,
    pub old_effects: &'a BTreeMap<SigId, EffectSet>,
    pub old_imports: &'a ImportMap,
    pub new_stages: &'a [Stage],
    pub new_imports: &'a ImportMap,
    pub diff: &'a DiffReport,
}

pub fn diff_to_ops(inputs: DiffInputs<'_>) -> Vec<OperationKind> {
    let mut out = Vec::new();
    let new_by_name: BTreeMap<&str, &Stage> = inputs.new_stages.iter()
        .filter_map(|s| {
            let n = match s {
                Stage::FnDecl(fd) => fd.name.as_str(),
                Stage::TypeDecl(td) => td.name.as_str(),
                Stage::Import(_) => return None,
            };
            Some((n, s))
        })
        .collect();

    // 1. Removed → RemoveFunction / RemoveType.
    for r in &inputs.diff.removed {
        // We need the SigId. Since `r.signature` is a rendered string,
        // not a SigId, look up the SigId via name in old_head's keys
        // by matching the stage history. Simpler: search old_head by
        // any sig whose name is `r.name` — but old_head doesn't carry
        // names. The CLI side will populate the inputs with the
        // SigIds directly; for diff_to_ops, we walk new vs old by
        // SigId. Re-read the diff: the CLI's compute_diff matches
        // by name. So we map name → SigId by walking the old-head
        // names retained somewhere. To keep this self-contained,
        // rely on the caller to also pass an old_names map.
        let _ = r; // (this branch implemented below in step 6)
    }
    out
}
```

Wait — the `DiffReport` keys things by *fn name*, but operations key things by `SigId`. There's a missing index. The cleanest fix: the caller (CLI) supplies a name → SigId index for the old-head side, and for the new-head side we compute `sig_id(stage)` directly.

Replace the `DiffInputs` struct and rewrite `diff_to_ops` accordingly. Replace the entire file with:

```rust
//! Convert a `DiffReport` (+ import set deltas + old head info)
//! into a sequence of typed operations.

use crate::diff_report::DiffReport;
use crate::operation::{EffectSet, ModuleRef, OperationKind, SigId, StageId};
use lex_ast::{sig_id, stage_id, Effect, Stage};
use std::collections::{BTreeMap, BTreeSet};

pub type ImportMap = BTreeMap<String, BTreeSet<ModuleRef>>;

#[derive(Debug)]
pub struct DiffInputs<'a> {
    /// Current head SigId → StageId map.
    pub old_head: &'a BTreeMap<SigId, StageId>,
    /// Map of fn/type *name* → its SigId at the current head. The
    /// caller assembles this by walking the old stages or the metadata.
    pub old_name_to_sig: &'a BTreeMap<String, SigId>,
    /// Effect set per sig at the current head.
    pub old_effects: &'a BTreeMap<SigId, EffectSet>,
    /// Per-file imports at the current head.
    pub old_imports: &'a ImportMap,
    /// Stages of the new program (post-canonicalize).
    pub new_stages: &'a [Stage],
    /// Per-file imports of the new program.
    pub new_imports: &'a ImportMap,
    /// AST-diff between old and new sources, by name.
    pub diff: &'a DiffReport,
}

pub fn diff_to_ops(inputs: DiffInputs<'_>) -> Vec<OperationKind> {
    let mut out = Vec::new();
    let new_by_name: BTreeMap<&str, &Stage> = inputs.new_stages.iter()
        .filter_map(|s| {
            let n = match s {
                Stage::FnDecl(fd) => fd.name.as_str(),
                Stage::TypeDecl(td) => td.name.as_str(),
                Stage::Import(_) => return None,
            };
            Some((n, s))
        })
        .collect();

    // 1. Imports — separate from stage ops; emit first so importer
    //    state is consistent before any sig ops apply.
    for (file, modules) in inputs.new_imports {
        let old = inputs.old_imports.get(file).cloned().unwrap_or_default();
        for m in modules.difference(&old) {
            out.push(OperationKind::AddImport {
                in_file: file.clone(),
                module: m.clone(),
            });
        }
        for m in old.difference(modules) {
            out.push(OperationKind::RemoveImport {
                in_file: file.clone(),
                module: m.clone(),
            });
        }
    }
    for (file, old) in inputs.old_imports {
        if !inputs.new_imports.contains_key(file) {
            for m in old {
                out.push(OperationKind::RemoveImport {
                    in_file: file.clone(),
                    module: m.clone(),
                });
            }
        }
    }

    // 2. Removed → RemoveFunction / RemoveType.
    for r in &inputs.diff.removed {
        let Some(sig) = inputs.old_name_to_sig.get(&r.name) else { continue; };
        let Some(last) = inputs.old_head.get(sig) else { continue; };
        // Decide fn vs type by looking up the stage signature shape.
        // We don't have the old AST here, so use a heuristic: type
        // sigs end in a TypeDecl-style suffix produced by sig_id —
        // but `lex_ast::sig_id` doesn't differentiate. Use the diff
        // signature string: `r.signature` for types starts with
        // "type ". Coarse but reliable for this codebase.
        if r.signature.starts_with("type ") {
            out.push(OperationKind::RemoveType {
                sig_id: sig.clone(),
                last_stage_id: last.clone(),
            });
        } else {
            out.push(OperationKind::RemoveFunction {
                sig_id: sig.clone(),
                last_stage_id: last.clone(),
            });
        }
    }

    // 3. Added → AddFunction / AddType.
    for a in &inputs.diff.added {
        let Some(stage) = new_by_name.get(a.name.as_str()) else { continue; };
        let Some(sig) = sig_id(stage) else { continue; };
        let Some(stg) = stage_id(stage) else { continue; };
        match stage {
            Stage::FnDecl(fd) => {
                let effects = effect_set(&fd.effects);
                out.push(OperationKind::AddFunction {
                    sig_id: sig, stage_id: stg, effects,
                });
            }
            Stage::TypeDecl(_) => {
                out.push(OperationKind::AddType { sig_id: sig, stage_id: stg });
            }
            Stage::Import(_) => unreachable!(),
        }
    }

    // 4. Renamed → RenameSymbol.
    for r in &inputs.diff.renamed {
        let Some(from_sig) = inputs.old_name_to_sig.get(&r.from) else { continue; };
        let Some(stage) = new_by_name.get(r.to.as_str()) else { continue; };
        let Some(to_sig) = sig_id(stage) else { continue; };
        let Some(body_id) = stage_id(stage) else { continue; };
        out.push(OperationKind::RenameSymbol {
            from: from_sig.clone(),
            to: to_sig,
            body_stage_id: body_id,
        });
    }

    // 5. Modified → ChangeEffectSig | ModifyBody | ModifyType.
    for m in &inputs.diff.modified {
        let Some(sig) = inputs.old_name_to_sig.get(&m.name) else { continue; };
        let Some(from_id) = inputs.old_head.get(sig) else { continue; };
        let Some(stage) = new_by_name.get(m.name.as_str()) else { continue; };
        let Some(to_id) = stage_id(stage) else { continue; };
        let effects_changed =
            !m.effect_changes.added.is_empty() || !m.effect_changes.removed.is_empty();
        match stage {
            Stage::FnDecl(fd) if effects_changed => {
                let from_effects = inputs.old_effects.get(sig).cloned().unwrap_or_default();
                let to_effects = effect_set(&fd.effects);
                out.push(OperationKind::ChangeEffectSig {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                    from_effects,
                    to_effects,
                });
            }
            Stage::FnDecl(_) => {
                out.push(OperationKind::ModifyBody {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                });
            }
            Stage::TypeDecl(_) => {
                out.push(OperationKind::ModifyType {
                    sig_id: sig.clone(),
                    from_stage_id: from_id.clone(),
                    to_stage_id: to_id,
                });
            }
            Stage::Import(_) => unreachable!(),
        }
    }

    out
}

fn effect_set(effs: &[Effect]) -> EffectSet {
    effs.iter().map(|e| e.kind.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_report::{AddRemove, DiffReport, EffectChanges, Modified, Renamed};

    fn dr() -> DiffReport { DiffReport::default() }

    fn empty_inputs<'a>() -> (
        BTreeMap<SigId, StageId>,
        BTreeMap<String, SigId>,
        BTreeMap<SigId, EffectSet>,
        ImportMap,
        ImportMap,
        Vec<Stage>,
        DiffReport,
    ) {
        Default::default()
    }

    #[test]
    fn empty_diff_yields_no_ops() {
        let (head, n2s, eff, oi, ni, stages, d) = empty_inputs();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_name_to_sig: &n2s,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &stages,
            new_imports: &ni,
            diff: &d,
        });
        assert!(ops.is_empty());
    }

    #[test]
    fn rename_emits_a_single_rename_op() {
        // Build a tiny new program with one fn under the new name.
        // We use lex_syntax + canonicalize_program for the fn so
        // sig_id/stage_id resolve.
        let src = "fn parse_int(s: Str) -> Int { 0 }";
        let prog = lex_syntax::load_program_from_str(src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let parse_int = stages.iter()
            .find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == "parse_int"))
            .cloned().unwrap();
        let to_sig = sig_id(&parse_int).unwrap();
        let to_stage = stage_id(&parse_int).unwrap();

        let mut head = BTreeMap::new();
        head.insert("parse-old-sig".to_string(), to_stage.clone());
        let mut n2s = BTreeMap::new();
        n2s.insert("parse".to_string(), "parse-old-sig".to_string());

        let mut diff = dr();
        diff.renamed.push(Renamed {
            from: "parse".into(),
            to: "parse_int".into(),
            signature: "fn parse_int(s :: Str) -> Int".into(),
        });

        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head,
            old_name_to_sig: &n2s,
            old_effects: &eff,
            old_imports: &oi,
            new_stages: &[parse_int],
            new_imports: &ni,
            diff: &diff,
        });
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::RenameSymbol { from, to, body_stage_id } => {
                assert_eq!(from, "parse-old-sig");
                assert_eq!(to, &to_sig);
                assert_eq!(body_stage_id, &to_stage);
            }
            other => panic!("expected RenameSymbol, got {other:?}"),
        }
    }

    #[test]
    fn body_only_modify_emits_modify_body() {
        let src = "fn fac(n: Int) -> Int { 1 }";
        let prog = lex_syntax::load_program_from_str(src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let fac = stages.iter().find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == "fac"))
            .cloned().unwrap();
        let sig = sig_id(&fac).unwrap();
        let new_stg = stage_id(&fac).unwrap();

        let mut head = BTreeMap::new();
        head.insert(sig.clone(), "old-stage-id".to_string());
        let mut n2s = BTreeMap::new();
        n2s.insert("fac".to_string(), sig.clone());

        let mut diff = dr();
        diff.modified.push(Modified {
            name: "fac".into(),
            signature_before: "fn fac(n :: Int) -> Int".into(),
            signature_after:  "fn fac(n :: Int) -> Int".into(),
            signature_changed: false,
            effect_changes: EffectChanges::default(),
            body_patches: Vec::new(),
        });

        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_name_to_sig: &n2s, old_effects: &eff,
            old_imports: &oi, new_stages: &[fac], new_imports: &ni, diff: &diff,
        });
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::ModifyBody { sig_id: s, from_stage_id, to_stage_id } => {
                assert_eq!(s, &sig);
                assert_eq!(from_stage_id, "old-stage-id");
                assert_eq!(to_stage_id, &new_stg);
            }
            other => panic!("expected ModifyBody, got {other:?}"),
        }
    }

    #[test]
    fn effect_change_emits_change_effect_sig() {
        let src = "fn shout(s: Str) -> Str ![io] { s }";
        let prog = lex_syntax::load_program_from_str(src).unwrap();
        let stages = lex_ast::canonicalize_program(&prog);
        let shout = stages.iter().find(|s| matches!(s, Stage::FnDecl(fd) if fd.name == "shout"))
            .cloned().unwrap();
        let sig = sig_id(&shout).unwrap();
        let new_stg = stage_id(&shout).unwrap();

        let mut head = BTreeMap::new();
        head.insert(sig.clone(), "old-stage".to_string());
        let mut n2s = BTreeMap::new();
        n2s.insert("shout".to_string(), sig.clone());
        let mut eff = BTreeMap::new();
        eff.insert(sig.clone(), BTreeSet::new()); // was pure

        let mut diff = dr();
        diff.modified.push(Modified {
            name: "shout".into(),
            signature_before: "fn shout(s :: Str) -> Str".into(),
            signature_after:  "fn shout(s :: Str) -> Str ![io]".into(),
            signature_changed: true,
            effect_changes: EffectChanges {
                before:  vec![],
                after:   vec!["io".into()],
                added:   vec!["io".into()],
                removed: vec![],
            },
            body_patches: Vec::new(),
        });

        let oi = ImportMap::new();
        let ni = ImportMap::new();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_name_to_sig: &n2s, old_effects: &eff,
            old_imports: &oi, new_stages: &[shout], new_imports: &ni, diff: &diff,
        });
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::ChangeEffectSig { sig_id: s, from_effects, to_effects, .. } => {
                assert_eq!(s, &sig);
                assert!(from_effects.is_empty());
                assert!(to_effects.contains("io"));
            }
            other => panic!("expected ChangeEffectSig, got {other:?}"),
        }
        let _ = new_stg; // silence unused-warning if test grows.
    }

    #[test]
    fn import_added_emits_add_import() {
        let mut new_imports = ImportMap::new();
        new_imports.insert("main.lex".into(),
            std::iter::once("std.io".to_string()).collect());
        let head = BTreeMap::new();
        let n2s = BTreeMap::new();
        let eff = BTreeMap::new();
        let oi = ImportMap::new();
        let stages: Vec<Stage> = Vec::new();
        let diff = dr();
        let ops = diff_to_ops(DiffInputs {
            old_head: &head, old_name_to_sig: &n2s, old_effects: &eff,
            old_imports: &oi, new_stages: &stages, new_imports: &new_imports, diff: &diff,
        });
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            OperationKind::AddImport { in_file, module } => {
                assert_eq!(in_file, "main.lex");
                assert_eq!(module, "std.io");
            }
            other => panic!("expected AddImport, got {other:?}"),
        }
    }
}
```

Update `crates/lex-vcs/src/lib.rs` to add the module + dev-deps for tests:

```rust
pub mod diff_report;
mod apply;
mod canonical;
pub mod diff_to_ops;
mod op_log;
mod operation;

pub use apply::{apply, ApplyError, NewHead};
pub use diff_to_ops::{diff_to_ops, DiffInputs, ImportMap};
pub use op_log::OpLog;
pub use operation::{
    EffectSet, ModuleRef, OpId, Operation, OperationRecord, OperationKind, SigId, StageId,
    StageTransition,
};
```

In `crates/lex-vcs/Cargo.toml` `[dev-dependencies]`, add:

```toml
lex-syntax = { path = "../lex-syntax" }
```

(needed by tests to parse the source strings).

- [ ] **Step 5: Run — confirm failures, then green**

Run: `cargo test -p lex-vcs diff_to_ops`
Expected: PASS — 5 tests green. (If failing on first run, the spec text in the test source — like effect syntax `![io]` — may need to match the parser. Adjust to the actual Lex source syntax used elsewhere; check `examples/` if needed.)

- [ ] **Step 6: Commit**

```bash
git add crates/lex-vcs/src/diff_report.rs crates/lex-vcs/src/diff_to_ops.rs \
        crates/lex-vcs/src/lib.rs crates/lex-vcs/Cargo.toml \
        crates/lex-cli/src/diff.rs
git commit -m "$(cat <<'EOF'
feat(lex-vcs): diff_to_ops converts ast-diff to typed op sequence

Moved DiffReport types into lex-vcs::diff_report so both producer
(lex-cli's ast-diff) and consumer (diff_to_ops) share the schema
without a cyclic dep.

Refs #129.
EOF
)"
```

---

## Task 7: Op-DAG merge engine + tier-1 merge tests

**Files:**
- Create: `crates/lex-vcs/src/merge.rs`
- Modify: `crates/lex-vcs/src/lib.rs`
- Modify: `crates/lex-store/src/branches.rs` (replace stub `merge`/`commit_merge` with the real op-DAG version)
- Modify: `crates/lex-store/tests/branches.rs` (rewrite using `apply_operation`)

The merge engine is a pure function over an `OpLog`. The `Store` wrapper produces the high-level `MergeReport` shape consumers already know.

- [ ] **Step 1: Write failing tests for the engine**

Create `crates/lex-vcs/src/merge.rs`:

```rust
//! Op-DAG three-way merge.
//!
//! 1. Compute LCA of src and dst heads.
//! 2. Get ops on each side since the LCA.
//! 3. Group by the `SigId` they touch; classify each group.

use crate::op_log::OpLog;
use crate::operation::{OpId, OperationKind, OperationRecord, SigId, StageId};
use std::collections::{BTreeMap, BTreeSet};
use std::io;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Both sides converged on the same op_id for this sig.
    Both { sig_id: SigId, stage_id: Option<StageId> },
    /// Only src touched it.
    Src  { sig_id: SigId, stage_id: Option<StageId> },
    /// Only dst touched it.
    Dst  { sig_id: SigId, stage_id: Option<StageId> },
    /// Conflict: both sides touched it with different ops.
    Conflict {
        sig_id: SigId,
        kind: ConflictKind,
        base: Option<StageId>,
        src:  Option<StageId>,
        dst:  Option<StageId>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    ModifyModify,
    ModifyDelete,
    DeleteModify,
    AddAdd,
}

#[derive(Debug)]
pub struct MergeOutput {
    pub lca: Option<OpId>,
    pub outcomes: Vec<MergeOutcome>,
}

pub fn merge(
    op_log: &OpLog,
    src_head: Option<&OpId>,
    dst_head: Option<&OpId>,
) -> io::Result<MergeOutput> {
    let lca = match (src_head, dst_head) {
        (Some(s), Some(d)) => op_log.lca(s, d)?,
        _ => None,
    };
    let src_ops = match src_head {
        Some(h) => op_log.ops_since(h, lca.as_ref())?,
        None => Vec::new(),
    };
    let dst_ops = match dst_head {
        Some(h) => op_log.ops_since(h, lca.as_ref())?,
        None => Vec::new(),
    };

    let src_by_sig = group_by_sig(&src_ops);
    let dst_by_sig = group_by_sig(&dst_ops);

    let lca_head: BTreeMap<SigId, StageId> = match lca.as_ref() {
        Some(id) => head_at(op_log, id)?,
        None => BTreeMap::new(),
    };

    let mut outcomes = Vec::new();
    let sigs: BTreeSet<&SigId> = src_by_sig.keys().chain(dst_by_sig.keys()).collect();
    for sig in sigs {
        let s = src_by_sig.get(sig);
        let d = dst_by_sig.get(sig);
        let s_stage = s.map(|recs| latest_stage(recs));
        let d_stage = d.map(|recs| latest_stage(recs));
        match (s, d) {
            (Some(s_recs), Some(d_recs)) => {
                let s_last = s_recs.last().map(|r| r.op_id.as_str()).unwrap_or("");
                let d_last = d_recs.last().map(|r| r.op_id.as_str()).unwrap_or("");
                if s_last == d_last {
                    outcomes.push(MergeOutcome::Both {
                        sig_id: sig.clone(),
                        stage_id: s_stage.unwrap(),
                    });
                } else {
                    let kind = classify(&s_stage.unwrap(), &d_stage.unwrap(), &lca_head, sig);
                    outcomes.push(MergeOutcome::Conflict {
                        sig_id: sig.clone(),
                        kind,
                        base: lca_head.get(sig).cloned(),
                        src:  latest_stage(s_recs),
                        dst:  latest_stage(d_recs),
                    });
                }
            }
            (Some(_), None) => {
                outcomes.push(MergeOutcome::Src {
                    sig_id: sig.clone(),
                    stage_id: s_stage.unwrap(),
                });
            }
            (None, Some(_)) => {
                outcomes.push(MergeOutcome::Dst {
                    sig_id: sig.clone(),
                    stage_id: d_stage.unwrap(),
                });
            }
            (None, None) => unreachable!(),
        }
    }

    Ok(MergeOutput { lca, outcomes })
}

fn group_by_sig(ops: &[OperationRecord]) -> BTreeMap<SigId, Vec<&OperationRecord>> {
    let mut out: BTreeMap<SigId, Vec<&OperationRecord>> = BTreeMap::new();
    for r in ops {
        if let Some(sig) = touched_sig(&r.op.kind) {
            out.entry(sig).or_default().push(r);
        }
    }
    // ops_since returned newest-first; reverse to oldest-first per sig
    // so `latest_stage` reads the right entry.
    for v in out.values_mut() { v.reverse(); }
    out
}

fn touched_sig(k: &OperationKind) -> Option<SigId> {
    match k {
        OperationKind::AddFunction { sig_id, .. }
        | OperationKind::RemoveFunction { sig_id, .. }
        | OperationKind::ModifyBody { sig_id, .. }
        | OperationKind::ChangeEffectSig { sig_id, .. }
        | OperationKind::AddType { sig_id, .. }
        | OperationKind::RemoveType { sig_id, .. }
        | OperationKind::ModifyType { sig_id, .. } => Some(sig_id.clone()),
        OperationKind::RenameSymbol { to, .. } => Some(to.clone()),
        OperationKind::AddImport { .. }
        | OperationKind::RemoveImport { .. }
        | OperationKind::Merge { .. } => None,
    }
}

/// Given a chronological (oldest-first) list of ops on a sig, return
/// the resulting stage_id (`None` if the sig was removed).
fn latest_stage(recs: &[&OperationRecord]) -> Option<StageId> {
    use crate::operation::StageTransition::*;
    let mut current: Option<StageId> = None;
    for r in recs {
        match &r.produces {
            Create { stage_id, .. } => current = Some(stage_id.clone()),
            Replace { to, .. } => current = Some(to.clone()),
            Remove { .. } => current = None,
            Rename { body_stage_id, .. } => current = Some(body_stage_id.clone()),
            ImportOnly | Merge { .. } => {}
        }
    }
    current
}

fn head_at(op_log: &OpLog, head: &OpId) -> io::Result<BTreeMap<SigId, StageId>> {
    let mut map = BTreeMap::new();
    for r in op_log.walk_forward(head, None)? {
        use crate::operation::StageTransition::*;
        match &r.produces {
            Create { sig_id, stage_id } => { map.insert(sig_id.clone(), stage_id.clone()); }
            Replace { sig_id, to, .. } => { map.insert(sig_id.clone(), to.clone()); }
            Remove { sig_id, .. } => { map.remove(sig_id); }
            Rename { from, to, body_stage_id } => {
                map.remove(from);
                map.insert(to.clone(), body_stage_id.clone());
            }
            ImportOnly => {}
            Merge { entries } => {
                for (sig, stage) in entries {
                    match stage {
                        Some(s) => { map.insert(sig.clone(), s.clone()); }
                        None    => { map.remove(sig); }
                    }
                }
            }
        }
    }
    Ok(map)
}

fn classify(
    src: &Option<StageId>,
    dst: &Option<StageId>,
    base: &BTreeMap<SigId, StageId>,
    sig: &SigId,
) -> ConflictKind {
    let in_base = base.contains_key(sig);
    match (in_base, src.is_some(), dst.is_some()) {
        (false, true, true)  => ConflictKind::AddAdd,
        (true,  true, true)  => ConflictKind::ModifyModify,
        (true,  true, false) => ConflictKind::ModifyDelete,
        (true,  false, true) => ConflictKind::DeleteModify,
        // Other combos shouldn't happen for a "both touched" group.
        _ => ConflictKind::ModifyModify,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::apply;
    use crate::operation::{Operation, OperationKind, StageTransition};
    use std::collections::BTreeSet;

    fn fresh() -> (OpLog, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        (OpLog::open(tmp.path()).unwrap(), tmp)
    }

    fn add_fn(log: &OpLog, parent: Option<&OpId>, sig: &str, stg: &str) -> OpId {
        let op = Operation::new(
            OperationKind::AddFunction {
                sig_id: sig.into(),
                stage_id: stg.into(),
                effects: BTreeSet::new(),
            },
            parent.cloned().into_iter().collect::<Vec<_>>(),
        );
        let t = StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() };
        apply(log, parent, op, t).unwrap().op_id
    }

    fn modify_body(log: &OpLog, parent: &OpId, sig: &str, from: &str, to: &str) -> OpId {
        let op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: sig.into(),
                from_stage_id: from.into(),
                to_stage_id: to.into(),
            },
            [parent.clone()],
        );
        let t = StageTransition::Replace {
            sig_id: sig.into(), from: from.into(), to: to.into(),
        };
        apply(log, Some(parent), op, t).unwrap().op_id
    }

    #[test]
    fn disjoint_sigs_merge_cleanly() {
        let (log, _tmp) = fresh();
        let root = add_fn(&log, None, "shared", "s0");
        let s_only = add_fn(&log, Some(&root), "src-only", "src1");
        let d_only = add_fn(&log, Some(&root), "dst-only", "dst1");

        let out = merge(&log, Some(&s_only), Some(&d_only)).unwrap();
        assert_eq!(out.lca.as_ref(), Some(&root));
        let kinds: Vec<&str> = out.outcomes.iter().map(|o| match o {
            MergeOutcome::Src { .. } => "src",
            MergeOutcome::Dst { .. } => "dst",
            MergeOutcome::Both { .. } => "both",
            MergeOutcome::Conflict { .. } => "conflict",
        }).collect();
        assert!(kinds.contains(&"src") && kinds.contains(&"dst"));
        assert!(!kinds.contains(&"conflict"));
    }

    #[test]
    fn same_sig_divergent_is_modify_modify_conflict() {
        let (log, _tmp) = fresh();
        let root = add_fn(&log, None, "fac", "s0");
        let src  = modify_body(&log, &root, "fac", "s0", "s-src");
        let dst  = modify_body(&log, &root, "fac", "s0", "s-dst");

        let out = merge(&log, Some(&src), Some(&dst)).unwrap();
        let conflict = out.outcomes.iter().find(|o| matches!(o, MergeOutcome::Conflict { .. }));
        assert!(conflict.is_some());
        if let Some(MergeOutcome::Conflict { kind, .. }) = conflict {
            assert!(matches!(kind, ConflictKind::ModifyModify));
        }
    }

    #[test]
    fn independent_histories_no_lca() {
        let (log, _tmp) = fresh();
        let a = add_fn(&log, None, "a", "sa");
        let b = add_fn(&log, None, "b", "sb");
        let out = merge(&log, Some(&a), Some(&b)).unwrap();
        assert!(out.lca.is_none());
    }
}
```

In `lib.rs`:

```rust
pub mod diff_report;
mod apply;
mod canonical;
pub mod diff_to_ops;
mod merge;
mod op_log;
mod operation;

pub use apply::{apply, ApplyError, NewHead};
pub use diff_to_ops::{diff_to_ops, DiffInputs, ImportMap};
pub use merge::{merge, ConflictKind, MergeOutcome, MergeOutput};
pub use op_log::OpLog;
pub use operation::{
    EffectSet, ModuleRef, OpId, Operation, OperationRecord, OperationKind, SigId, StageId,
    StageTransition,
};
```

- [ ] **Step 2: Run — confirm failures, then green**

Run: `cargo test -p lex-vcs merge`
Expected: PASS — three tests green.

- [ ] **Step 3: Wire `Store::merge` and `commit_merge` to the engine**

In `crates/lex-store/src/branches.rs`, replace the two stubs with:

```rust
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
                lex_vcs::MergeOutcome::Both { sig_id, stage_id }
                | lex_vcs::MergeOutcome::Src  { sig_id, stage_id }
                | lex_vcs::MergeOutcome::Dst  { sig_id, stage_id } => {
                    let from: &'static str = match _from_label(&o) {
                        Label::Both => "both",
                        Label::Src  => "src",
                        Label::Dst  => "dst",
                    };
                    if let Some(stage_id) = stage_id {
                        report.merged.push(MergeEntry { sig_id, stage_id, from });
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
        // Build the merge op's StageTransition::Merge entries: every
        // sig in `merged` whose stage differs from the dst-current head.
        let dst_head_map = self.branch_head(dst)?;
        let mut entries: BTreeMap<String, Option<String>> = BTreeMap::new();
        for m in &report.merged {
            let cur = dst_head_map.get(&m.sig_id);
            if cur != Some(&m.stage_id) {
                entries.insert(m.sig_id.clone(), Some(m.stage_id.clone()));
            }
        }
        // No-op merge: nothing changed relative to dst — emit a marker
        // op anyway so the merge appears in the log (parents=[src,dst]).
        let src_head = self.get_branch(&report.summary.src)?.and_then(|b| b.head_op);
        let dst_head = self.get_branch(dst)?.and_then(|b| b.head_op);
        let parents: Vec<_> = [src_head, dst_head].into_iter().flatten().collect();
        let op = lex_vcs::Operation::new(
            lex_vcs::OperationKind::Merge { resolved: entries.len() },
            parents,
        );
        let t = lex_vcs::StageTransition::Merge { entries };
        let _ = self.apply_operation(dst, op, t)?;

        // Journal the merge for `lex log`.
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

#[derive(Debug)]
enum Label { Both, Src, Dst }

fn _from_label(o: &lex_vcs::MergeOutcome) -> Label {
    match o {
        lex_vcs::MergeOutcome::Both { .. } => Label::Both,
        lex_vcs::MergeOutcome::Src  { .. } => Label::Src,
        lex_vcs::MergeOutcome::Dst  { .. } => Label::Dst,
        lex_vcs::MergeOutcome::Conflict { .. } => unreachable!(),
    }
}
```

(`OpLog` import is already at the top of `branches.rs`; `lex_vcs` aliases come for free.)

- [ ] **Step 4: Replace the placeholder branch tests**

In `crates/lex-store/tests/branches.rs`, replace the placeholder with:

```rust
//! Branch tests over the op-DAG model.

use lex_store::{Operation, OperationKind, StageTransition, Store, DEFAULT_BRANCH};
use std::collections::BTreeSet;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let s = Store::open(tmp.path()).unwrap();
    (s, tmp)
}

fn add(s: &Store, branch: &str, sig: &str, stg: &str) -> String {
    let op = Operation::new(
        OperationKind::AddFunction {
            sig_id: sig.into(),
            stage_id: stg.into(),
            effects: BTreeSet::new(),
        },
        s.get_branch(branch).unwrap().and_then(|b| b.head_op).into_iter().collect::<Vec<_>>(),
    );
    let t = StageTransition::Create { sig_id: sig.into(), stage_id: stg.into() };
    s.apply_operation(branch, op, t).unwrap()
}

fn modify(s: &Store, branch: &str, sig: &str, from: &str, to: &str) -> String {
    let parent = s.get_branch(branch).unwrap().and_then(|b| b.head_op).unwrap();
    let op = Operation::new(
        OperationKind::ModifyBody {
            sig_id: sig.into(),
            from_stage_id: from.into(),
            to_stage_id: to.into(),
        },
        [parent],
    );
    let t = StageTransition::Replace {
        sig_id: sig.into(), from: from.into(), to: to.into(),
    };
    s.apply_operation(branch, op, t).unwrap()
}

#[test]
fn fresh_store_lists_only_main() {
    let (s, _tmp) = fresh();
    assert_eq!(s.list_branches().unwrap(), vec![DEFAULT_BRANCH.to_string()]);
    assert_eq!(s.current_branch(), DEFAULT_BRANCH);
}

#[test]
fn create_branch_inherits_head_op() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature-x", DEFAULT_BRANCH).unwrap();
    assert_eq!(
        s.branch_head("feature-x").unwrap().get("sig1"),
        Some(&"stageA".to_string()),
    );
}

#[test]
fn merge_clean_when_only_one_side_modifies() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, "feature", "sig1", "stageA", "stageB");
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 0, "report: {report:?}");
    assert_eq!(report.merged.len(), 1);
    assert_eq!(report.merged[0].stage_id, "stageB");
}

#[test]
fn merge_conflict_when_both_sides_modify_same_sig() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, DEFAULT_BRANCH, "sig1", "stageA", "stageB");
    let _ = modify(&s, "feature",      "sig1", "stageA", "stageC");
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].kind, "modify-modify");
}

#[test]
fn commit_merge_advances_dst_head_op() {
    let (s, _tmp) = fresh();
    let _ = add(&s, DEFAULT_BRANCH, "sig1", "stageA");
    s.create_branch("feature", DEFAULT_BRANCH).unwrap();
    let _ = modify(&s, "feature", "sig1", "stageA", "stageB");
    let report = s.merge("feature", DEFAULT_BRANCH).unwrap();
    s.commit_merge(DEFAULT_BRANCH, &report).unwrap();
    assert_eq!(
        s.branch_head(DEFAULT_BRANCH).unwrap().get("sig1"),
        Some(&"stageB".to_string()),
    );
    assert_eq!(s.branch_log(DEFAULT_BRANCH).unwrap().len(), 1);
}

#[test]
fn delete_branch_refused_when_current_or_default() {
    let (s, _tmp) = fresh();
    s.create_branch("foo", DEFAULT_BRANCH).unwrap();
    s.set_current_branch("foo").unwrap();
    assert!(s.delete_branch("foo").is_err());
    assert!(s.delete_branch(DEFAULT_BRANCH).is_err());
    s.set_current_branch(DEFAULT_BRANCH).unwrap();
    s.delete_branch("foo").unwrap();
    assert_eq!(s.list_branches().unwrap(), vec![DEFAULT_BRANCH.to_string()]);
}
```

- [ ] **Step 5: Run all lex-store tests**

Run: `cargo test -p lex-store`
Expected: PASS — all of `m6.rs`, `branches.rs`, `apply_operation.rs` green.

- [ ] **Step 6: Commit**

```bash
git add crates/lex-vcs/src/merge.rs crates/lex-vcs/src/lib.rs \
        crates/lex-store/src/branches.rs crates/lex-store/tests/branches.rs
git commit -m "$(cat <<'EOF'
feat(lex-vcs,lex-store): op-DAG merge engine + tier-1 wiring

Three-way merge keys on (LCA, ops_since); conflict shape preserved
(modify-modify / modify-delete / delete-modify / add-add). Store::merge
and Store::commit_merge route through it; commit_merge produces a
real Merge op with two parents, advancing dst.head_op atomically.

Branch tests rewritten to drive merges via apply_operation.

Refs #129.
EOF
)"
```

---

## Task 8: `lex publish` refactor

**Files:**
- Modify: `crates/lex-cli/src/main.rs`

The new flow: parse → CLI-side type-check → diff against current branch head → diff_to_ops → for each op, publish stage AST + `apply_operation`.

- [ ] **Step 1: Add an integration test**

Create `crates/lex-cli/tests/publish.rs`:

```rust
//! `lex publish` over the op-DAG model.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

#[test]
fn publish_creates_main_branch_with_head_op() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n: Int) -> Int { 1 }\n").unwrap();
    let out = Command::new(lex_bin())
        .args([
            "--output", "json",
            "publish",
            "--store", store.path().to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output()
        .expect("run publish");
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).expect("ops field");
    assert!(ops.is_array());
    assert!(!ops.as_array().unwrap().is_empty(), "expected at least one op");
    // branches/main.json exists after the first publish.
    assert!(store.path().join("branches/main.json").exists(),
        "main branch file should exist post-publish");
}

#[test]
fn republish_unchanged_source_emits_zero_ops() {
    let store = tempdir().unwrap();
    let src = store.path().join("a.lex");
    std::fs::write(&src, "fn fac(n: Int) -> Int { 1 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src.to_str().unwrap()])
        .output().unwrap();
    let out = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ops = v.pointer("/data/ops").or_else(|| v.get("ops")).expect("ops field");
    assert_eq!(ops.as_array().unwrap().len(), 0, "expected 0 ops on no-op republish");
}
```

- [ ] **Step 2: Run — confirm failure**

Run: `cargo test -p lex-cli --test publish`
Expected: FAIL — current `lex publish` doesn't emit `ops`/`head_op`.

- [ ] **Step 3: Refactor `cmd_publish`**

In `crates/lex-cli/src/main.rs`, replace the `cmd_publish` body. The implementation is long; carefully replace from line 681 ("fn cmd_publish") through the closing brace, with:

```rust
fn cmd_publish(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    use lex_vcs::{
        diff_to_ops, DiffInputs, ImportMap, OperationKind, Operation, StageTransition,
    };
    use std::collections::{BTreeMap, BTreeSet};

    let (root, rest, activate, dry_run) = parse_store_flag(args);
    // Pull --branch off as well.
    let mut branch: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else {
            positional.push(a.clone());
        }
    }
    let path = positional.first().ok_or_else(|| anyhow!(
        "usage: lex publish [--store DIR] [--branch NAME] [--activate] <file>"))?;

    let prog = read_program(path)?;
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        let arr: Vec<serde_json::Value> = errs.iter()
            .map(|e| serde_json::to_value(e).unwrap()).collect();
        let data = serde_json::json!({ "phase": "type-check", "errors": arr });
        acli::emit_or_text("publish", data, fmt, || {
            for e in &errs {
                if let Ok(j) = serde_json::to_string(e) { eprintln!("{j}"); }
            }
        });
        std::process::exit(2);
    }

    let store = Store::open(&root).with_context(|| format!("opening store at {}", root.display()))?;
    let branch = branch.unwrap_or_else(|| store.current_branch());

    // Build "old" view from the current branch head.
    let old_head = store.branch_head(&branch)?;
    let old_name_to_sig = old_head.iter()
        .filter_map(|(sig, stg)| {
            store.get_metadata(stg).ok().map(|m| (m.name, sig.clone()))
        })
        .collect::<BTreeMap<String, String>>();
    let old_effects = old_head.iter()
        .filter_map(|(sig, stg)| {
            let ast = store.get_ast(stg).ok()?;
            match ast {
                Stage::FnDecl(fd) => {
                    let s: BTreeSet<String> = fd.effects.iter()
                        .map(|e| e.kind.clone()).collect();
                    Some((sig.clone(), s))
                }
                _ => None,
            }
        })
        .collect::<BTreeMap<_, _>>();
    // Imports — derive from the op log on this branch.
    let old_imports: ImportMap = derive_imports_from_oplog(&store, &branch)?;

    // Compute the ast-diff.
    let old_stages = old_head.values()
        .filter_map(|stg| store.get_ast(stg).ok())
        .collect::<Vec<_>>();
    let report = diff::compute_diff(&old_stages, &stages, /* body_patches: */ true);

    // Build new imports map (one entry per source file we just read).
    let mut new_imports: ImportMap = ImportMap::new();
    let file_key = std::path::PathBuf::from(path).file_name()
        .map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| path.clone());
    let entry = new_imports.entry(file_key).or_default();
    for s in &stages {
        if let Stage::Import(im) = s {
            entry.insert(im.path.clone());
        }
    }

    let op_kinds = diff_to_ops(DiffInputs {
        old_head: &old_head,
        old_name_to_sig: &old_name_to_sig,
        old_effects: &old_effects,
        old_imports: &old_imports,
        new_stages: &stages,
        new_imports: &new_imports,
        diff: &report,
    });

    if dry_run {
        let data = serde_json::json!({
            "ops": op_kinds.iter().map(|k| serde_json::to_value(k).unwrap())
                .collect::<Vec<_>>(),
        });
        acli::emit_dry_run("publish", fmt,
            &format!("would apply {} op(s) to branch {}", op_kinds.len(), branch),
            data.as_object().cloned().map(|m| m.into_iter().map(|(_,v)| v).collect()).unwrap_or_default());
        return Ok(());
    }

    // For each op: persist any new stage AST/metadata, then apply_operation.
    let mut emitted: Vec<serde_json::Value> = Vec::new();
    let mut last_op_id: Option<String> = None;
    for kind in op_kinds {
        // Find the corresponding new stage (if any) and publish it via
        // the existing Store::publish path so the AST/metadata files exist.
        let stage_to_publish = stage_for_kind(&kind, &stages);
        if let Some(stg) = stage_to_publish {
            store.publish(stg).with_context(|| "publishing stage")?;
            if activate {
                if let Some(stage_id_str) = lex_ast::stage_id(stg) {
                    store.activate(&stage_id_str).ok();
                }
            }
        }
        let transition = transition_for_kind(&kind);
        let head_now = store.get_branch(&branch)?.and_then(|b| b.head_op);
        let op = Operation::new(kind.clone(), head_now.into_iter().collect::<Vec<_>>());
        let op_id = store.apply_operation(&branch, op, transition)?;
        emitted.push(serde_json::json!({
            "op_id": op_id,
            "kind": serde_json::to_value(&kind)?,
        }));
        last_op_id = Some(op_id);
    }

    let data = serde_json::json!({
        "ops": emitted,
        "head_op": last_op_id,
    });
    acli::emit_or_text("publish", data, fmt, || {
        // Empty closure: text mode prints the JSON envelope.
    });
    Ok(())
}

fn stage_for_kind<'a>(kind: &OperationKind, stages: &'a [Stage]) -> Option<&'a Stage> {
    use OperationKind::*;
    let target_sig = match kind {
        AddFunction { sig_id, .. } | ModifyBody { sig_id, .. }
        | ChangeEffectSig { sig_id, .. } | AddType { sig_id, .. }
        | ModifyType { sig_id, .. } => Some(sig_id.clone()),
        RenameSymbol { to, .. } => Some(to.clone()),
        _ => None,
    };
    let target_sig = target_sig?;
    stages.iter().find(|s| lex_ast::sig_id(s).as_deref() == Some(target_sig.as_str()))
}

fn transition_for_kind(kind: &OperationKind) -> StageTransition {
    use OperationKind::*;
    match kind {
        AddFunction { sig_id, stage_id, .. }
        | AddType { sig_id, stage_id } => StageTransition::Create {
            sig_id: sig_id.clone(), stage_id: stage_id.clone(),
        },
        RemoveFunction { sig_id, last_stage_id }
        | RemoveType { sig_id, last_stage_id } => StageTransition::Remove {
            sig_id: sig_id.clone(), last: last_stage_id.clone(),
        },
        ModifyBody { sig_id, from_stage_id, to_stage_id }
        | ChangeEffectSig { sig_id, from_stage_id, to_stage_id, .. }
        | ModifyType { sig_id, from_stage_id, to_stage_id } => StageTransition::Replace {
            sig_id: sig_id.clone(),
            from: from_stage_id.clone(),
            to:   to_stage_id.clone(),
        },
        RenameSymbol { from, to, body_stage_id } => StageTransition::Rename {
            from: from.clone(), to: to.clone(),
            body_stage_id: body_stage_id.clone(),
        },
        AddImport { .. } | RemoveImport { .. } => StageTransition::ImportOnly,
        Merge { .. } => StageTransition::Merge { entries: Default::default() },
    }
}

fn derive_imports_from_oplog(
    store: &Store,
    branch: &str,
) -> Result<lex_vcs::ImportMap> {
    use lex_vcs::OperationKind::*;
    let log = lex_vcs::OpLog::open(store.root())?;
    let head = match store.get_branch(branch)?.and_then(|b| b.head_op) {
        Some(h) => h, None => return Ok(Default::default()),
    };
    let mut out: lex_vcs::ImportMap = Default::default();
    for r in log.walk_forward(&head, None)? {
        match r.op.kind {
            AddImport { in_file, module } => {
                out.entry(in_file).or_default().insert(module);
            }
            RemoveImport { in_file, module } => {
                if let Some(set) = out.get_mut(&in_file) { set.remove(&module); }
            }
            _ => {}
        }
    }
    Ok(out)
}
```

In the same file, also expose `compute_diff` from `diff.rs` as `pub(crate)`. In `crates/lex-cli/src/diff.rs`, find `fn compute_diff(...)` and make it `pub(crate)`:

```rust
pub(crate) fn compute_diff(a: &[Stage], b: &[Stage], body_patches: bool) -> DiffReport {
    // ... existing body ...
}
```

If `compute_diff` currently takes `&[FnDecl]` instead of `&[Stage]`, you'll need a tiny adapter — read the function as it currently exists and adapt the call site rather than mutating the function (it's used by `cmd_diff`).

- [ ] **Step 4: Run the integration tests**

Run: `cargo test -p lex-cli --test publish`
Expected: PASS.

Run: `cargo test -p lex-cli` (the rest of the CLI tests)
Expected: PASS — `ast_diff`, `ast_merge`, etc. still green.

- [ ] **Step 5: Commit**

```bash
git add crates/lex-cli/src/main.rs crates/lex-cli/src/diff.rs crates/lex-cli/tests/publish.rs
git commit -m "$(cat <<'EOF'
feat(lex-cli): publish emits typed operation sequence

Reads current branch head, diffs against new source, converts
diff to op kinds, applies each via Store::apply_operation. No-op
publish emits zero ops with success exit. CLI-side type-check
preview retained pending #130's gate.

Refs #129.
EOF
)"
```

---

## Task 9: `lex blame` refactor

**Files:**
- Modify: `crates/lex-cli/src/main.rs`

`lex blame` walks the op log on the current branch and surfaces a per-fn `causal_history` array alongside the existing `history` (which is lifecycle-based and stays).

- [ ] **Step 1: Add a failing test**

Append to `crates/lex-cli/tests/publish.rs`:

```rust
#[test]
fn blame_after_rename_shows_one_causal_event() {
    let store = tempdir().unwrap();
    let src1 = store.path().join("a.lex");
    std::fs::write(&src1, "fn parse(s: Str) -> Int { 0 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();
    // Rename: same body, new name.
    std::fs::write(&src1, "fn parse_int(s: Str) -> Int { 0 }\n").unwrap();
    let _ = Command::new(lex_bin())
        .args(["--output","json","publish","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();

    let out = Command::new(lex_bin())
        .args(["--output","json","blame","--store",store.path().to_str().unwrap(),src1.to_str().unwrap()])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let blame = v.pointer("/data/blame").or_else(|| v.get("blame")).unwrap();
    let parse_int = blame.as_array().unwrap().iter()
        .find(|e| e["name"] == "parse_int").expect("parse_int in blame");
    let causal = parse_int["causal_history"].as_array().expect("causal_history");
    let renames: Vec<_> = causal.iter()
        .filter(|e| e["kind"] == "rename_symbol").collect();
    assert_eq!(renames.len(), 1, "expected exactly one rename in causal history");
}
```

- [ ] **Step 2: Run — confirm failure**

Run: `cargo test -p lex-cli --test publish blame_after_rename`
Expected: FAIL.

- [ ] **Step 3: Refactor `cmd_blame`**

In `crates/lex-cli/src/main.rs`, find `cmd_blame` (around line 530). After the existing `entries.push(...)` block adds the legacy fields, also walk the op log:

```rust
        // ... existing entries.push call ...

        // New: causal history from the op log.
        let log = lex_vcs::OpLog::open(store.root()).ok();
        let head_op = store.get_branch(&store.current_branch()).ok()
            .and_then(|opt| opt.and_then(|b| b.head_op));
        let causal: Vec<serde_json::Value> = match (log, head_op) {
            (Some(log), Some(head)) => {
                log.walk_back(&head, None).unwrap_or_default()
                    .into_iter()
                    .filter(|r| {
                        // Touch this sig (or, for renames, produce it as the new sig).
                        match &r.op.kind {
                            lex_vcs::OperationKind::AddFunction { sig_id, .. }
                            | lex_vcs::OperationKind::ModifyBody { sig_id, .. }
                            | lex_vcs::OperationKind::ChangeEffectSig { sig_id, .. }
                            | lex_vcs::OperationKind::AddType { sig_id, .. }
                            | lex_vcs::OperationKind::ModifyType { sig_id, .. }
                            | lex_vcs::OperationKind::RemoveFunction { sig_id, .. }
                            | lex_vcs::OperationKind::RemoveType { sig_id, .. } => sig_id == &sig,
                            lex_vcs::OperationKind::RenameSymbol { from, to, .. } =>
                                from == &sig || to == &sig,
                            _ => false,
                        }
                    })
                    .map(|r| {
                        let kind_tag = serde_json::to_value(&r.op.kind).ok()
                            .and_then(|v| v.get("op").cloned())
                            .unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "op_id": r.op_id,
                            "kind": kind_tag,
                        })
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        // Mutate the most-recent entries.push value to attach causal_history.
        if let Some(last) = entries.last_mut() {
            last.as_object_mut().unwrap()
                .insert("causal_history".into(), serde_json::Value::Array(causal));
        }
```

(Insert this block inside the per-stage loop, after the existing `entries.push(...)`.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p lex-cli --test publish`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/lex-cli/src/main.rs crates/lex-cli/tests/publish.rs
git commit -m "$(cat <<'EOF'
feat(lex-cli): blame surfaces causal_history from the op log

Per-fn blame entries now carry a causal_history array of ops
that touched the sig (including renames as a single event).
Existing lifecycle-based history is preserved.

Refs #129.
EOF
)"
```

---

## Task 10: `lex op show` and `lex op log`

**Files:**
- Create: `crates/lex-cli/src/op.rs`
- Modify: `crates/lex-cli/src/main.rs`
- Create: `crates/lex-cli/tests/op.rs`

- [ ] **Step 1: Write the failing tests**

`crates/lex-cli/tests/op.rs`:

```rust
//! `lex op show` and `lex op log`.

use std::process::Command;
use tempfile::tempdir;

fn lex_bin() -> &'static str { env!("CARGO_BIN_EXE_lex") }

fn publish(store: &std::path::Path, src: &std::path::Path) {
    let out = Command::new(lex_bin())
        .args([
            "--output","json","publish","--store",store.to_str().unwrap(),
            src.to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn op_log_walks_branch_head_back() {
    let store = tempdir().unwrap();
    let a = store.path().join("a.lex");
    std::fs::write(&a, "fn fac(n: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &a);
    std::fs::write(&a, "fn fac(n: Int) -> Int { 2 }\n").unwrap();
    publish(store.path(), &a);

    let out = Command::new(lex_bin())
        .args([
            "--output","json","op","log",
            "--store", store.path().to_str().unwrap(),
        ])
        .output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = v.pointer("/data/log").or_else(|| v.get("log")).unwrap();
    assert!(entries.as_array().unwrap().len() >= 2);
}

#[test]
fn op_show_returns_record() {
    let store = tempdir().unwrap();
    let a = store.path().join("a.lex");
    std::fs::write(&a, "fn fac(n: Int) -> Int { 1 }\n").unwrap();
    publish(store.path(), &a);

    let log_out = Command::new(lex_bin())
        .args(["--output","json","op","log","--store",store.path().to_str().unwrap()])
        .output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&log_out.stdout).unwrap();
    let entries = v.pointer("/data/log").or_else(|| v.get("log")).unwrap();
    let first = &entries.as_array().unwrap()[0];
    let op_id = first["op_id"].as_str().unwrap().to_string();

    let show_out = Command::new(lex_bin())
        .args(["--output","json","op","show","--store",store.path().to_str().unwrap(), &op_id])
        .output().unwrap();
    assert!(show_out.status.success(), "stderr: {}", String::from_utf8_lossy(&show_out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&show_out.stdout).unwrap();
    let rec = v.pointer("/data/op").or_else(|| v.get("op")).unwrap();
    assert_eq!(rec["op_id"].as_str().unwrap(), op_id);
}
```

- [ ] **Step 2: Run — confirm failure**

Run: `cargo test -p lex-cli --test op`
Expected: FAIL — `op` subcommand doesn't exist.

- [ ] **Step 3: Implement the subcommand**

Create `crates/lex-cli/src/op.rs`:

```rust
//! `lex op show` and `lex op log`.

use crate::acli;
use ::acli::OutputFormat;
use anyhow::{anyhow, bail, Result};
use lex_store::Store;
use lex_vcs::{OpLog, OperationRecord};
use std::path::PathBuf;

fn parse_store(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root: Option<PathBuf> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--store" {
            if let Some(p) = it.next() { root = Some(PathBuf::from(p)); }
        } else {
            rest.push(a.clone());
        }
    }
    let root = root.unwrap_or_else(|| {
        let home = std::env::var("HOME").map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".lex/store")
    });
    (root, rest)
}

pub fn cmd_op(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let sub = args.first().ok_or_else(|| anyhow!(
        "usage: lex op {{show|log}} [--store DIR] ..."))?;
    let rest = &args[1..];
    match sub.as_str() {
        "show" => cmd_op_show(fmt, rest),
        "log"  => cmd_op_log(fmt, rest),
        other  => bail!("unknown `lex op` subcommand: {other}"),
    }
}

fn cmd_op_show(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let op_id = rest.first().ok_or_else(|| anyhow!(
        "usage: lex op show [--store DIR] <op_id>"))?;
    let log = OpLog::open(&root)?;
    let rec = log.get(op_id)?
        .ok_or_else(|| anyhow!("unknown op_id: {op_id}"))?;
    let data = serde_json::json!({ "op": serde_json::to_value(&rec)? });
    acli::emit_or_text("op", data, fmt, || render_record(&rec));
    Ok(())
}

fn cmd_op_log(fmt: &OutputFormat, args: &[String]) -> Result<()> {
    let (root, rest) = parse_store(args);
    let mut branch: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        if a == "--branch" {
            branch = Some(it.next().ok_or_else(|| anyhow!("--branch needs a value"))?.clone());
        } else if a == "--limit" {
            limit = Some(it.next().ok_or_else(|| anyhow!("--limit needs N"))?
                .parse().map_err(|e| anyhow!("--limit: {e}"))?);
        }
    }
    let store = Store::open(&root)?;
    let branch = branch.unwrap_or_else(|| store.current_branch());
    let head = store.get_branch(&branch)?
        .and_then(|b| b.head_op);
    let log = OpLog::open(&root)?;
    let recs = match head {
        Some(h) => log.walk_back(&h, limit)?,
        None => Vec::new(),
    };
    let arr: Vec<serde_json::Value> = recs.iter()
        .map(|r| serde_json::to_value(r).unwrap()).collect();
    let data = serde_json::json!({ "log": arr, "branch": branch });
    acli::emit_or_text("op", data, fmt, || {
        for r in &recs { render_record(r); }
    });
    Ok(())
}

fn render_record(r: &OperationRecord) {
    println!("op_id:   {}", r.op_id);
    let kind_label = serde_json::to_value(&r.op.kind).ok()
        .and_then(|v| v.get("op").and_then(|s| s.as_str().map(str::to_string)))
        .unwrap_or_else(|| "?".into());
    println!("kind:    {kind_label}");
    if r.op.parents.is_empty() {
        println!("parents: (none)");
    } else {
        for p in &r.op.parents {
            println!("parent:  {p}");
        }
    }
    println!();
}
```

In `crates/lex-cli/src/main.rs`, register the module + dispatch:

```rust
mod op;
```

Add to the dispatcher in `run`:

```rust
        "op" => op::cmd_op(fmt, &args[1..]),
```

(Insert it next to the other subcommand routings, alphabetically.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p lex-cli --test op`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/lex-cli/src/op.rs crates/lex-cli/src/main.rs crates/lex-cli/tests/op.rs
git commit -m "$(cat <<'EOF'
feat(lex-cli): lex op show and lex op log

op show <op_id> dumps an OperationRecord; op log walks the op DAG
from the current (or --branch) head, optionally bounded by --limit.
JSON envelope: { log: [...], branch: NAME } and { op: ... }.

Refs #129.
EOF
)"
```

---

## Task 11: `lex-api` publish/patch handlers route through `apply_operation`

**Files:**
- Modify: `crates/lex-api/src/handlers.rs`
- Modify: `crates/lex-api/Cargo.toml` (add `lex-vcs` dep)

The HTTP API's `/v1/publish` and `/v1/patch` currently call `store.publish` directly. Change them to drive `Store::apply_operation` so the op log is the single advance path.

- [ ] **Step 1: Add lex-vcs dep**

In `crates/lex-api/Cargo.toml`:

```toml
lex-vcs = { path = "../lex-vcs" }
```

- [ ] **Step 2: Refactor `publish_handler`**

Replace the body of `publish_handler` with a call to a new helper that mirrors the CLI's diff→ops→apply flow. Cleanest split: factor the CLI's flow into a small library function in a new module and call it from both. Pragmatic option for now: duplicate the smaller-than-CLI logic inline since the API payload is a single source string against the store's current branch.

Replace the existing `publish_handler` body (around line 132 of `handlers.rs`) with the same shape used in `cmd_publish` from Task 8: build `DiffInputs`, call `diff_to_ops`, loop `apply_operation`. The output JSON shape becomes:

```json
{
  "ops": [{ "op_id": "...", "kind": {...} }],
  "head_op": "..."
}
```

Concretely, replace the function body inside the `for s in &stages` loop region: instead of `store.publish(s)` followed by `store.activate(...)`, build the op kinds via `diff_to_ops`, persist stages via `store.publish(s)` (still needed — the AST/metadata files), then call `store.apply_operation`. Output the new JSON.

If wholesale duplication of the CLI logic feels heavy, extract a `Store::publish_program(branch, stages, activate) -> Result<PublishOutcome, StoreError>` helper into `lex-store/src/store.rs` so both the CLI and API call it. Strongly preferred:

```rust
// crates/lex-store/src/store.rs (new method)
impl Store {
    pub fn publish_program(
        &self,
        branch: &str,
        stages: &[lex_ast::Stage],
        new_imports: &lex_vcs::ImportMap,
        activate: bool,
    ) -> Result<PublishOutcome, StoreError> {
        // (factor the per-op loop from cmd_publish here)
        // returns: { ops: Vec<{op_id, kind}>, head_op: Option<OpId> }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct PublishOutcome {
    pub ops: Vec<serde_json::Value>,
    pub head_op: Option<lex_vcs::OpId>,
}
```

Then `cmd_publish` in Task 8 collapses to "parse, type-check, build new_imports, call `store.publish_program`." If you took the inline-in-CLI route in Task 8, refactor it into this helper as part of Task 11 — both call sites now use the helper.

- [ ] **Step 3: Refactor `patch_handler`**

`patch_handler` applies an AST patch to a stored stage. It also currently calls `store.publish(&patched)` + `store.activate`. Change it to call `store.publish_program` with `[patched]` against the current branch. The activate flag is forwarded to `publish_program`.

The handler's output JSON gains an `op_id` field:

```json
{
  "old_stage_id": "...",
  "new_stage_id": "...",
  "sig_id": "...",
  "status": "draft|active|...",
  "op_id": "..."
}
```

- [ ] **Step 4: Run lex-api tests**

Run: `cargo test -p lex-api`
Expected: PASS — existing `lex-api` tests don't pin the JSON shape too tightly; if any break, update them to read from `head_op` / `ops[0].op_id`.

If `lex-api` doesn't have its own integration tests today, the manual verification is: run `lex serve` against a temp store, POST to `/v1/publish`, confirm the response includes `ops` and `head_op`, and inspect `<store>/ops/`.

- [ ] **Step 5: Commit**

```bash
git add crates/lex-api/src/handlers.rs crates/lex-api/Cargo.toml \
        crates/lex-store/src/store.rs crates/lex-cli/src/main.rs
git commit -m "$(cat <<'EOF'
feat(lex-api): publish/patch handlers route through apply_operation

Extracted Store::publish_program helper used by both cli and api.
HTTP /v1/publish and /v1/patch now produce ops[] + head_op fields.

Refs #129.
EOF
)"
```

---

## Task 12: README + status row + integration smoke test

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Update the Status table**

Replace the "Agent-native version control" row at line 513 of README.md with:

```
| Agent-native version control | tier-1 ✅ — `lex branch` + structured JSON conflicts ; **tier-2 ✅ — operation log as source of truth (`lex publish` emits typed ops, `lex op show` / `lex op log`, `lex blame` causal history)** ; write-time type-check gate (#130) next ; distributed sync deferred |
```

(Adjust the wording to match nearby rows' tone.)

- [ ] **Step 2: Bump the test count line**

Re-run the workspace tests to find the new total:

```bash
cargo test --workspace 2>&1 | tail -5
```

Update the line near 517: `**Workspace test count:** N passing, ...` to the new N. (The exact number depends on local test outcomes; just plug in the cargo output.)

- [ ] **Step 3: Final workspace test sweep**

Run: `cargo test --workspace`
Expected: PASS — entire workspace green.

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS — no warnings.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "$(cat <<'EOF'
docs(README): tier-2 op-log status; bump test count

Refs #129.
EOF
)"
```

---

## Self-Review

Spec section coverage:

- §"Why replace": addressed in Tasks 4 + 7 (schema rewrite, merge engine).
- §"Scope" in: every task hits one or more in-scope items; nothing in the "explicitly out of scope" list is implemented.
- §"Data model — `lex-vcs` modules": Tasks 2 (op_log), 3 (apply), 6 (diff_to_ops), 7 (merge).
- §"Data model — `lex-store` data model changes" (new `Branch`, `branch_head` computed): Task 4.
- §"Data model — Storage layout" (`<root>/ops/<op_id>.json`): Task 2.
- §"Runtime — apply flow": Tasks 5 (Store::apply_operation), 8 (lex publish wiring), 11 (API).
- §"Runtime — atomicity": Tasks 2 (`OpLog::put`), 4 (`set_branch_head_op` via `write_branch_atomic`).
- §"Runtime — idempotency": Task 8 (no-op publish test) + Task 2 (idempotent `put`).
- §"CLI surface — `lex op show`/`lex op log`": Task 10.
- §"CLI surface — `lex publish` refactor": Task 8.
- §"CLI surface — `lex blame` refactor": Task 9.
- §"CLI surface — `lex store-merge`": Task 7 (Store::merge re-wired without changing CLI flags).
- §"Conformance" — re-interpreted as integration tests in `lex-vcs` and `lex-cli/tests/`. The spec mentions JSON descriptors per `OperationKind`; the `conformance/` crate is a runtime-execution harness, not a kind-schema mechanism. The integration tests in Task 8 + 9 + 10 cover each kind in practice (rename produces one event etc.). If schema-descriptor JSON files are wanted, they'd be a follow-up task; this plan doesn't ship them.
- §"Tests": all eight integration tests are covered across Tasks 5, 7, 8, 9, 10.
- §"Acceptance" criteria from #129: emit op sequence (Task 8), independent runs same op_id (already covered in Task 1's existing test), rename = one op (Task 9 test), `lex op show`/`log` documented (Task 10 + Task 12 README), kinds covered by tests (Tasks 6 + 8).

Type consistency:

- `OpId`, `SigId`, `StageId` are `String` aliases throughout. No accidental `&str` vs `String` mismatches.
- `Operation::new` always sorts/dedups parents (existing behavior).
- `StageTransition::Merge { entries }` uses `BTreeMap<SigId, Option<StageId>>` — same shape in `branches.rs::apply_transition` (Task 4) and `merge.rs::head_at` (Task 7).
- `OperationKind::Merge { resolved }` named identically across Tasks 1, 7.
- `apply_operation` returns `OpId` in both `Store` impl (Task 5) and the test calls (Tasks 5, 7).
- `DiffInputs` field names match between `diff_to_ops.rs` (Task 6) and `cmd_publish` call site (Task 8).

Placeholder scan: no TBDs. The closest thing to a soft spot is Task 11's "if you took the inline-in-CLI route in Task 8, refactor it into this helper" — that's a deliberate refactoring instruction, not a placeholder. Task 8's last block also touches `compute_diff` signature; the executor should read the existing function and adapt the call site if it currently takes `&[FnDecl]` (a clear instruction, not a TBD).

Spec scope: single coherent change, single PR, ~12 buildable steps. No decomposition needed.

---

## Plan complete

Plan saved to `docs/superpowers/plans/2026-05-04-issue-129-operation-log.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints for review.

Which approach?
