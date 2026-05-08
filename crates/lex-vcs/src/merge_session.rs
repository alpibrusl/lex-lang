//! Stateful merge sessions for programmatic conflict resolution (#134).
//!
//! Today's `lex_vcs::merge` returns a list of `MergeOutcome`s — auto-
//! merged sigs *and* conflicts — and exits. To act on conflicts an
//! agent has to:
//!
//! 1. Run `lex store-merge`.
//! 2. Parse the JSON output.
//! 3. Decide a resolution per conflict.
//! 4. Manually edit source files.
//! 5. Run `lex check`.
//! 6. Run `lex publish`.
//! 7. Loop on failure.
//!
//! Six round-trips for what should be one transaction. Worse, the
//! agent edits *text* between steps 4 and 6 — the typed conflict
//! the merge engine produced gets re-derived from the new text. The
//! information loss is what the issue calls out.
//!
//! [`MergeSession`] gives the engine layer needed to expose merging
//! as a state machine: `start` collects conflicts, `resolve` accepts
//! batched [`Resolution`]s, `commit` finalizes when no conflicts
//! remain. The HTTP wrapper (`POST /v1/merge/start` etc.) and the
//! CLI mirror (`lex merge resolve`) compose on top of this.
//!
//! # Why a stateful session
//!
//! Merging conflicts iteratively is the natural agent loop:
//! "submit 50 resolutions, see which were accepted, fix the ones
//! that broke type-checking, retry." The session holds the
//! in-progress state so the merge cost (LCA computation, op
//! grouping, conflict classification) is paid once per merge,
//! not once per resolution batch.
//!
//! # What's in the foundation slice
//!
//! The state machine: types, transitions, validation hook for
//! resolved candidates, commit path that produces a fresh head op.
//! Persistence (so a session survives a process restart) and the
//! HTTP / CLI surfaces are subsequent slices.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::merge::{ConflictKind, MergeOutcome, MergeOutput};
use crate::op_log::OpLog;
use crate::operation::{OpId, Operation, SigId, StageId};

/// Stable id for a merge in flight. Caller-supplied so the HTTP
/// surface can map URLs to sessions without leaking session ids
/// from the engine. Production callers will likely use UUIDs;
/// tests use short strings.
pub type MergeSessionId = String;

/// Stable id for a conflict within a session. We use the SigId as
/// the conflict id since conflicts are 1:1 with the sigs that have
/// `MergeOutcome::Conflict`. If a future merge ever produces
/// multiple conflicts on the same sig, this becomes a tuple.
pub type ConflictId = SigId;

/// Snapshot of one conflict the agent needs to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub conflict_id: ConflictId,
    pub sig_id: SigId,
    pub kind: ConflictKind,
    /// Stage on the LCA. `None` for `AddAdd` (no shared base) and
    /// for sigs that didn't exist on the LCA.
    pub base: Option<StageId>,
    /// Stage on the dst (ours) side of the merge. `None` if dst
    /// removed it.
    pub ours: Option<StageId>,
    /// Stage on the src (theirs) side of the merge. `None` if src
    /// removed it.
    pub theirs: Option<StageId>,
}

/// Choice for a single conflict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    /// Keep dst's stage; discard src's.
    TakeOurs,
    /// Keep src's stage; discard dst's.
    TakeTheirs,
    /// Submit a brand-new op that supersedes both sides. The op's
    /// parents must include both ours and theirs (the merge engine
    /// validates this; see [`MergeSession::validate_resolution`]).
    Custom { op: Operation },
    /// Punt to a human reviewer. Surfaces as
    /// [`CommitError::ConflictsRemaining`] on commit until removed.
    Defer,
}

/// Why a resolution was rejected. Distinct from [`CommitError`]
/// because a resolve call returns *per-conflict* verdicts; commit
/// returns a single overall verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolutionRejection {
    /// The conflict_id doesn't refer to any pending conflict in
    /// the session. Either the agent invented one, or it was
    /// already resolved and the session pruned it.
    UnknownConflict { conflict_id: ConflictId },
    /// The custom op's parents don't include both `ours` and
    /// `theirs`. A custom resolution that doesn't acknowledge
    /// both sides isn't a merge — it's a fork.
    CustomOpMissingParents {
        conflict_id: ConflictId,
        expected: Vec<OpId>,
        got: Vec<OpId>,
    },
}

/// Per-conflict outcome of a resolve call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveVerdict {
    pub conflict_id: ConflictId,
    pub accepted: bool,
    pub rejection: Option<ResolutionRejection>,
}

/// Why a commit failed. Conflicts-remaining is the most common
/// case — agents are expected to iterate via resolve until this
/// goes away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    /// At least one conflict has no resolution or has
    /// [`Resolution::Defer`]. The session is still alive; submit
    /// resolutions and retry.
    ConflictsRemaining(Vec<ConflictId>),
}

/// Stateful merge in flight. Hold one per active merge between
/// `start` and `commit`. Sessions are not thread-safe; the HTTP
/// wrapper is expected to wrap them in a `Mutex` keyed by
/// [`MergeSessionId`].
#[derive(Debug, Serialize, Deserialize)]
pub struct MergeSession {
    pub merge_id: MergeSessionId,
    pub src_head: Option<OpId>,
    pub dst_head: Option<OpId>,
    pub lca: Option<OpId>,
    /// Outcomes the engine resolved unilaterally — `Both` (both
    /// sides agreed) and one-sided (`Src` / `Dst`). The agent sees
    /// these for audit but doesn't need to act on them.
    pub auto_resolved: Vec<MergeOutcome>,
    /// Conflicts indexed by id. Removed as resolutions land.
    conflicts: BTreeMap<ConflictId, ConflictRecord>,
    /// Resolutions accumulated across resolve calls. Validated
    /// against `conflicts` when applied.
    resolutions: BTreeMap<ConflictId, Resolution>,
}

impl MergeSession {
    /// Start a merge session. Runs the engine in [`crate::merge`]
    /// and partitions the outcomes into auto-resolved and
    /// conflicts-needing-attention.
    pub fn start(
        merge_id: impl Into<MergeSessionId>,
        op_log: &OpLog,
        src_head: Option<&OpId>,
        dst_head: Option<&OpId>,
    ) -> std::io::Result<Self> {
        let MergeOutput { lca, outcomes } = crate::merge::merge(op_log, src_head, dst_head)?;
        let mut auto_resolved = Vec::new();
        let mut conflicts: BTreeMap<ConflictId, ConflictRecord> = BTreeMap::new();
        for outcome in outcomes {
            match outcome {
                MergeOutcome::Conflict {
                    sig_id,
                    kind,
                    base,
                    src,
                    dst,
                } => {
                    let conflict_id = sig_id.clone();
                    conflicts.insert(
                        conflict_id.clone(),
                        ConflictRecord {
                            conflict_id,
                            sig_id,
                            kind,
                            base,
                            // The merge engine returns `src` and
                            // `dst` from src's and dst's perspective
                            // respectively. We map dst→ours and
                            // src→theirs, matching the canonical
                            // git terminology and the issue text.
                            ours: dst,
                            theirs: src,
                        },
                    );
                }
                other => auto_resolved.push(other),
            }
        }
        Ok(Self {
            merge_id: merge_id.into(),
            src_head: src_head.cloned(),
            dst_head: dst_head.cloned(),
            lca,
            auto_resolved,
            conflicts,
            resolutions: BTreeMap::new(),
        })
    }

    /// Pending conflicts (those without a non-defer resolution).
    pub fn remaining_conflicts(&self) -> Vec<&ConflictRecord> {
        self.conflicts
            .values()
            .filter(|c| {
                !matches!(self.resolutions.get(&c.conflict_id),
                    Some(Resolution::TakeOurs)
                    | Some(Resolution::TakeTheirs)
                    | Some(Resolution::Custom { .. }))
            })
            .collect()
    }

    /// Submit resolutions in batch. Returns one verdict per input.
    /// Accepted resolutions are recorded; rejected ones leave the
    /// previous resolution (if any) in place so partial submissions
    /// don't clobber earlier good work.
    pub fn resolve(
        &mut self,
        resolutions: Vec<(ConflictId, Resolution)>,
    ) -> Vec<ResolveVerdict> {
        let mut out = Vec::with_capacity(resolutions.len());
        for (conflict_id, resolution) in resolutions {
            match self.validate_resolution(&conflict_id, &resolution) {
                Ok(()) => {
                    self.resolutions.insert(conflict_id.clone(), resolution);
                    out.push(ResolveVerdict {
                        conflict_id,
                        accepted: true,
                        rejection: None,
                    });
                }
                Err(rej) => {
                    out.push(ResolveVerdict {
                        conflict_id,
                        accepted: false,
                        rejection: Some(rej),
                    });
                }
            }
        }
        out
    }

    /// Validate a single resolution against the session's pending
    /// conflicts. Pure (no side effects); the caller decides
    /// whether to accept.
    pub fn validate_resolution(
        &self,
        conflict_id: &ConflictId,
        resolution: &Resolution,
    ) -> Result<(), ResolutionRejection> {
        if !self.conflicts.contains_key(conflict_id) {
            return Err(ResolutionRejection::UnknownConflict { conflict_id: conflict_id.clone() });
        }
        if let Resolution::Custom { op } = resolution {
            // Validate that the custom op's parent set acknowledges
            // both sides. We don't have direct OpIds for the
            // ours/theirs ops here (the conflict record carries
            // stage ids), so the check is "the op has at least two
            // parents" — a stronger check requires looking up the
            // ops by sig and confirming they're in the parents,
            // which is a follow-up enhancement.
            //
            // For the foundation slice this catches the obvious
            // misuse (`Operation::new(kind, [])`) without
            // reconstructing the merge engine's own validation.
            if op.parents.len() < 2 {
                return Err(ResolutionRejection::CustomOpMissingParents {
                    conflict_id: conflict_id.clone(),
                    expected: vec!["ours-op-id".into(), "theirs-op-id".into()],
                    got: op.parents.clone(),
                });
            }
        }
        Ok(())
    }

    /// Finalize the merge. On success returns the resolved
    /// resolutions in conflict_id order. The caller is responsible
    /// for synthesizing the final `Operation::Merge` op against the
    /// store and persisting it; this function returns the engine's
    /// view of "what to land," not the persisted op id.
    pub fn commit(self) -> Result<Vec<(ConflictId, Resolution)>, CommitError> {
        let unresolved: Vec<ConflictId> = self
            .conflicts
            .keys()
            .filter(|id| {
                !matches!(self.resolutions.get(*id),
                    Some(Resolution::TakeOurs)
                    | Some(Resolution::TakeTheirs)
                    | Some(Resolution::Custom { .. }))
            })
            .cloned()
            .collect();
        if !unresolved.is_empty() {
            return Err(CommitError::ConflictsRemaining(unresolved));
        }
        let mut resolved: Vec<(ConflictId, Resolution)> = self.resolutions.into_iter().collect();
        resolved.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operation::{OperationKind, OperationRecord, StageTransition};
    use std::collections::BTreeSet;

    /// Tiny fixture: one branch (dst) modifies fn::A from stage-0 to
    /// stage-1; another (src) modifies fn::A to stage-2. The LCA is
    /// the original add. The merge surfaces a `ModifyModify`
    /// conflict on fn::A.
    fn fixture() -> (tempfile::TempDir, OpLog, OpId, OpId) {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let r0 = OperationRecord::new(
            Operation::new(
                OperationKind::AddFunction {
                    sig_id: "fn::A".into(),
                    stage_id: "stage-0".into(),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                },
                [],
            ),
            StageTransition::Create {
                sig_id: "fn::A".into(),
                stage_id: "stage-0".into(),
            },
        );
        log.put(&r0).unwrap();

        let r1 = OperationRecord::new(
            Operation::new(
                OperationKind::ModifyBody {
                    sig_id: "fn::A".into(),
                    from_stage_id: "stage-0".into(),
                    to_stage_id: "stage-1".into(),
                    from_budget: None,
                    to_budget: None,
                },
                [r0.op_id.clone()],
            ),
            StageTransition::Replace {
                sig_id: "fn::A".into(),
                from: "stage-0".into(),
                to: "stage-1".into(),
            },
        );
        log.put(&r1).unwrap();

        let r2 = OperationRecord::new(
            Operation::new(
                OperationKind::ModifyBody {
                    sig_id: "fn::A".into(),
                    from_stage_id: "stage-0".into(),
                    to_stage_id: "stage-2".into(),
                    from_budget: None,
                    to_budget: None,
                },
                [r0.op_id.clone()],
            ),
            StageTransition::Replace {
                sig_id: "fn::A".into(),
                from: "stage-0".into(),
                to: "stage-2".into(),
            },
        );
        log.put(&r2).unwrap();

        (tmp, log, r1.op_id, r2.op_id)
    }

    #[test]
    fn start_collects_conflicts() {
        let (_tmp, log, dst, src) = fixture();
        let session =
            MergeSession::start("ms-1", &log, Some(&src), Some(&dst)).unwrap();
        assert_eq!(session.remaining_conflicts().len(), 1);
        assert_eq!(session.remaining_conflicts()[0].sig_id, "fn::A");
        assert_eq!(
            session.remaining_conflicts()[0].kind,
            ConflictKind::ModifyModify
        );
        assert_eq!(
            session.remaining_conflicts()[0].ours.as_deref(),
            Some("stage-1"),
        );
        assert_eq!(
            session.remaining_conflicts()[0].theirs.as_deref(),
            Some("stage-2"),
        );
        assert_eq!(
            session.remaining_conflicts()[0].base.as_deref(),
            Some("stage-0"),
        );
    }

    #[test]
    fn no_conflicts_when_branches_dont_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let r0 = OperationRecord::new(
            Operation::new(
                OperationKind::AddFunction {
                    sig_id: "fn::A".into(),
                    stage_id: "stage-0".into(),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                },
                [],
            ),
            StageTransition::Create {
                sig_id: "fn::A".into(),
                stage_id: "stage-0".into(),
            },
        );
        log.put(&r0).unwrap();
        let r1 = OperationRecord::new(
            Operation::new(
                OperationKind::AddFunction {
                    sig_id: "fn::B".into(),
                    stage_id: "stage-B".into(),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                },
                [r0.op_id.clone()],
            ),
            StageTransition::Create {
                sig_id: "fn::B".into(),
                stage_id: "stage-B".into(),
            },
        );
        log.put(&r1).unwrap();

        let session =
            MergeSession::start("ms-2", &log, Some(&r1.op_id), Some(&r0.op_id)).unwrap();
        assert!(session.remaining_conflicts().is_empty());
        assert_eq!(session.auto_resolved.len(), 1, "fn::B added on src side");
    }

    #[test]
    fn resolve_take_ours_clears_conflict() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-3", &log, Some(&src), Some(&dst)).unwrap();
        let verdicts = session.resolve(vec![("fn::A".into(), Resolution::TakeOurs)]);
        assert_eq!(verdicts.len(), 1);
        assert!(verdicts[0].accepted);
        assert!(session.remaining_conflicts().is_empty());
    }

    #[test]
    fn resolve_take_theirs_clears_conflict() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-4", &log, Some(&src), Some(&dst)).unwrap();
        let verdicts =
            session.resolve(vec![("fn::A".into(), Resolution::TakeTheirs)]);
        assert!(verdicts[0].accepted);
        assert!(session.remaining_conflicts().is_empty());
    }

    #[test]
    fn resolve_unknown_conflict_is_rejected() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-5", &log, Some(&src), Some(&dst)).unwrap();
        let verdicts =
            session.resolve(vec![("fn::Z".into(), Resolution::TakeOurs)]);
        assert_eq!(verdicts.len(), 1);
        assert!(!verdicts[0].accepted);
        assert!(matches!(
            verdicts[0].rejection,
            Some(ResolutionRejection::UnknownConflict { .. }),
        ));
    }

    #[test]
    fn custom_op_without_two_parents_is_rejected() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-6", &log, Some(&src), Some(&dst)).unwrap();
        // A custom op with empty parents — clearly not a merge.
        let bad_op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fn::A".into(),
                from_stage_id: "stage-0".into(),
                to_stage_id: "stage-X".into(),
                from_budget: None,
                to_budget: None,
            },
            [],
        );
        let verdicts = session.resolve(vec![(
            "fn::A".into(),
            Resolution::Custom { op: bad_op },
        )]);
        assert!(!verdicts[0].accepted);
        assert!(matches!(
            verdicts[0].rejection,
            Some(ResolutionRejection::CustomOpMissingParents { .. }),
        ));
        // The conflict is still pending — bad resolutions don't
        // clobber the slot.
        assert_eq!(session.remaining_conflicts().len(), 1);
    }

    #[test]
    fn custom_op_with_two_parents_is_accepted() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-7", &log, Some(&src), Some(&dst)).unwrap();
        let merge_op = Operation::new(
            OperationKind::ModifyBody {
                sig_id: "fn::A".into(),
                from_stage_id: "stage-0".into(),
                to_stage_id: "stage-merged".into(),
                from_budget: None,
                to_budget: None,
            },
            [src.clone(), dst.clone()],
        );
        let verdicts = session.resolve(vec![(
            "fn::A".into(),
            Resolution::Custom { op: merge_op },
        )]);
        assert!(verdicts[0].accepted);
        assert!(session.remaining_conflicts().is_empty());
    }

    #[test]
    fn defer_keeps_conflict_pending() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-8", &log, Some(&src), Some(&dst)).unwrap();
        let verdicts = session.resolve(vec![("fn::A".into(), Resolution::Defer)]);
        // Defer is a valid resolution — accepted — but the conflict
        // stays in `remaining_conflicts` since it still requires
        // human attention.
        assert!(verdicts[0].accepted);
        assert_eq!(session.remaining_conflicts().len(), 1);
    }

    #[test]
    fn commit_with_no_conflicts_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let session = MergeSession::start("ms-9", &log, None, None).unwrap();
        let resolved = session.commit().unwrap();
        assert!(resolved.is_empty());
    }

    #[test]
    fn commit_with_unresolved_conflict_fails() {
        let (_tmp, log, dst, src) = fixture();
        let session =
            MergeSession::start("ms-10", &log, Some(&src), Some(&dst)).unwrap();
        let err = session.commit().unwrap_err();
        match err {
            CommitError::ConflictsRemaining(ids) => {
                assert_eq!(ids, vec!["fn::A".to_string()]);
            }
        }
    }

    #[test]
    fn commit_with_defer_remaining_fails() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-11", &log, Some(&src), Some(&dst)).unwrap();
        session.resolve(vec![("fn::A".into(), Resolution::Defer)]);
        let err = session.commit().unwrap_err();
        match err {
            CommitError::ConflictsRemaining(ids) => {
                assert_eq!(ids, vec!["fn::A".to_string()]);
            }
        }
    }

    #[test]
    fn commit_after_resolve_succeeds() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-12", &log, Some(&src), Some(&dst)).unwrap();
        session.resolve(vec![("fn::A".into(), Resolution::TakeOurs)]);
        let resolved = session.commit().unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "fn::A");
        assert!(matches!(resolved[0].1, Resolution::TakeOurs));
    }

    #[test]
    fn batch_resolve_accepts_partial() {
        // Mixed batch: one valid, one referencing an unknown
        // conflict. The valid one should land; the bad one should
        // be rejected without clobbering anything else.
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-13", &log, Some(&src), Some(&dst)).unwrap();
        let verdicts = session.resolve(vec![
            ("fn::A".into(), Resolution::TakeOurs),
            ("fn::DOESNT_EXIST".into(), Resolution::TakeTheirs),
        ]);
        assert_eq!(verdicts.len(), 2);
        assert!(verdicts[0].accepted);
        assert!(!verdicts[1].accepted);
        // fn::A is now resolved.
        assert!(session.remaining_conflicts().is_empty());
    }

    #[test]
    fn auto_resolved_outcomes_are_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        // Single branch: just an add; no second branch to merge,
        // but `MergeSession::start(... None ...)` still runs the
        // engine. This documents what `auto_resolved` carries.
        let r0 = OperationRecord::new(
            Operation::new(
                OperationKind::AddFunction {
                    sig_id: "fn::A".into(),
                    stage_id: "stage-0".into(),
                    effects: BTreeSet::new(),
                    budget_cost: None,
                },
                [],
            ),
            StageTransition::Create {
                sig_id: "fn::A".into(),
                stage_id: "stage-0".into(),
            },
        );
        log.put(&r0).unwrap();
        let session =
            MergeSession::start("ms-14", &log, Some(&r0.op_id), None).unwrap();
        assert!(session.remaining_conflicts().is_empty());
        // src had a unique op vs the missing dst → it's an Src
        // outcome surfaced as auto-resolved.
        assert_eq!(session.auto_resolved.len(), 1);
    }
}
