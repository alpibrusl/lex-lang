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
use crate::operation::{BlobId, OpId, Operation, SigId, StageId};

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

// ---- Files (#1007 PR 7): the manifest side of a merge ----
//
// `lex-vcs` doesn't know about `Manifest` or blob storage (that's
// `lex-store`'s layer — see `crate::merge_session`'s module docs on
// `ResolutionChecker` for the same layering reason). So a file conflict
// here is described purely in terms of the blob triple a path resolved
// to on each side — everything a caller needs to render or resolve it,
// without this crate depending on `lex-store::files::{Entry, Manifest}`.
// The caller (lex-store, via `Store::manifest_merge`) computes the 3-way
// diff and hands the resulting conflicts to
// [`MergeSession::attach_file_conflicts`]; this crate then tracks
// resolutions the same way it tracks sig conflicts.

/// A path in a files manifest (#1007). Distinct type alias from
/// [`ConflictId`] even though both are `String` — a sig id and a file
/// path are never interchangeable, and the alias documents which one a
/// signature expects.
pub type FilePath = String;

/// One side of a [`FileConflict`]: the blob (and its metadata) a path
/// resolved to in a manifest. Mirrors `lex_store::files::Entry` field
/// for field, without this crate depending on `lex-store`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub blob: BlobId,
    pub mode: String,
    pub size: u64,
}

/// A path whose manifest entry differs on both sides of a merge, and
/// differs from the base too — a real content conflict, not a one-sided
/// edit (those auto-resolve before this is ever surfaced; see
/// `Store::manifest_merge`'s doc comment for the auto-resolve rule).
///
/// Blobs are opaque bytes (binary allowed, per #1007 §2) so there is no
/// meaningful 3-way *content* merge the way there is for text lines —
/// resolving a `FileConflict` means picking a side
/// ([`FileResolution::TakeOurs`] / [`FileResolution::TakeTheirs`]), not
/// splicing bytes. See the module docs on why real content merging is
/// out of scope for #1007 PR 7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileConflict {
    pub path: FilePath,
    /// The path's entry on the merge base (the LCA's manifest). `None`
    /// if the path didn't exist there (both sides added it
    /// differently — the file-level analogue of `ConflictKind::AddAdd`).
    pub base: Option<FileEntry>,
    /// The path's entry on dst's side. `None` if dst doesn't have it
    /// (removed, or never added).
    pub ours: Option<FileEntry>,
    /// The path's entry on src's side. `None` if src doesn't have it.
    pub theirs: Option<FileEntry>,
}

/// Choice for a single file conflict. No `Custom` variant (unlike
/// [`Resolution`]): a file conflict has no "brand-new op" analogue — the
/// only two things you can do with two divergent versions of an opaque
/// blob are keep one or the other. See the module docs above.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileResolution {
    /// Keep dst's (ours) entry for this path; discard src's.
    TakeOurs,
    /// Keep src's (theirs) entry for this path; discard dst's.
    TakeTheirs,
    /// Punt to a human reviewer, same as [`Resolution::Defer`].
    Defer,
}

/// Why a file resolution was rejected. Only the structural case applies
/// today — a file resolution never fails a type-check the way a sig
/// resolution can, since `SetFiles` carries no program semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileResolutionRejection {
    /// `path` doesn't refer to any pending file conflict in the
    /// session — invented, or already resolved and pruned.
    UnknownConflict { path: FilePath },
}

/// Per-path outcome of a `resolve_files` call. Mirrors [`ResolveVerdict`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileResolveVerdict {
    pub path: FilePath,
    pub accepted: bool,
    pub rejection: Option<FileResolutionRejection>,
}

/// Choice for a single conflict.
// `Operation` is the only payload-carrying variant and grew with
// #280's typed transforms. Clippy flags the size disparity, but
// boxing the field would churn callers (HTTP handler, CLI, tests)
// for a heuristic warning — the heap allocation cost vs. the
// occasional empty variant is not actually a hot path here.
#[allow(clippy::large_enum_variant)]
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
    /// The resolution is structurally valid but the program it
    /// produces — dst's head with this resolution (and every
    /// resolution accepted so far) overlaid — does not type-check.
    /// Only returned by [`MergeSession::resolve_checked`]; the
    /// structural [`MergeSession::resolve`] never composes a program
    /// and so never emits this. `errors` are the composed program's
    /// type errors, rendered by the injected [`ResolutionChecker`].
    TypeError {
        conflict_id: ConflictId,
        errors: Vec<String>,
    },
}

/// Injected composer + type-checker for merge resolutions.
///
/// `lex-vcs` deliberately does not depend on `lex-store`, so a merge
/// session cannot compose a program from stage ids on its own — it
/// only knows the *shape* of the merge (which sig resolves to which
/// stage). The caller, which holds the store, supplies a checker so
/// [`MergeSession::resolve_checked`] can type-check a resolution the
/// moment it is submitted rather than only at commit. This mirrors
/// [`crate::IntentResolver`], the same dependency-injection seam the
/// predicate engine uses.
///
/// Implementors receive the full projected post-merge **delta against
/// dst's head** — `sig_id -> Some(stage)` to set that sig to `stage`,
/// `sig_id -> None` to remove it. The implementor overlays the delta
/// onto dst's current head, composes the stages, and type-checks:
/// return the (possibly empty) list of type errors as strings. An
/// empty vec means the resolution composes.
pub trait ResolutionChecker {
    fn typecheck_projection(&self, delta: &BTreeMap<SigId, Option<StageId>>) -> Vec<String>;
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
    /// At least one sig conflict has no resolution or has
    /// [`Resolution::Defer`]. The session is still alive; submit
    /// resolutions and retry. Checked before file conflicts, so a
    /// session with both kinds pending reports this first.
    ConflictsRemaining(Vec<ConflictId>),
    /// At least one file conflict (#1007 PR 7) has no resolution or
    /// has [`FileResolution::Defer`]. Only reachable once
    /// `ConflictsRemaining` is empty.
    FileConflictsRemaining(Vec<FilePath>),
}

/// What [`MergeSession::commit`] hands the caller to land: the resolved
/// sig conflicts (as before #1007) plus the resolved file conflicts and
/// whether the merge needs a `SetFiles` op at all (#1007 PR 7 — see
/// [`MergeSession::attach_file_conflicts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeCommitOutput {
    pub resolved: Vec<(ConflictId, Resolution)>,
    pub resolved_files: Vec<(FilePath, FileResolution)>,
    /// True when dst's and src's files manifests disagreed at all (even
    /// if every path auto-resolved with no [`FileConflict`]) — the
    /// merge commit must append a `SetFiles` recording the merged
    /// manifest, per #1007 §1. False when the session was never told
    /// about a files dimension, or the manifests already agreed.
    pub needs_setfiles: bool,
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
    /// File conflicts (#1007 PR 7), attached separately from `start`
    /// via [`Self::attach_file_conflicts`] — `lex-vcs` doesn't compute
    /// these itself (see the module docs above `FilePath`). Empty for
    /// a session whose merge has no files dimension at all.
    #[serde(default)]
    file_conflicts: BTreeMap<FilePath, FileConflict>,
    /// File resolutions accumulated across `resolve_files` calls.
    #[serde(default)]
    file_resolutions: BTreeMap<FilePath, FileResolution>,
    /// Whether dst's and src's files manifests disagree at all — see
    /// [`MergeCommitOutput::needs_setfiles`].
    #[serde(default)]
    needs_setfiles: bool,
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
            file_conflicts: BTreeMap::new(),
            file_resolutions: BTreeMap::new(),
            needs_setfiles: false,
        })
    }

    /// Attach the files dimension of the merge (#1007 PR 7): the
    /// conflicts a 3-way manifest diff surfaced (paths edited
    /// differently on both sides — see [`FileConflict`]) and whether
    /// the merge needs a `SetFiles` op at all. Called once, right
    /// after [`Self::start`], by the caller that holds the store (the
    /// same layering [`ResolutionChecker`] uses: this crate tracks the
    /// session's state machine, the caller computes the domain-specific
    /// diff). A no-op call with an empty `conflicts` and
    /// `needs_setfiles: false` — the default — leaves a session with
    /// no files dimension, exactly as before this feature existed.
    pub fn attach_file_conflicts(&mut self, conflicts: Vec<FileConflict>, needs_setfiles: bool) {
        self.file_conflicts = conflicts.into_iter().map(|c| (c.path.clone(), c)).collect();
        self.needs_setfiles = needs_setfiles;
    }

    /// Whether the merge needs a `SetFiles` op appended on commit, per
    /// [`MergeCommitOutput::needs_setfiles`].
    pub fn needs_setfiles(&self) -> bool {
        self.needs_setfiles
    }

    /// Pending file conflicts (those without a non-defer resolution).
    /// Mirrors [`Self::remaining_conflicts`].
    pub fn remaining_file_conflicts(&self) -> Vec<&FileConflict> {
        self.file_conflicts
            .values()
            .filter(|c| {
                !matches!(
                    self.file_resolutions.get(&c.path),
                    Some(FileResolution::TakeOurs) | Some(FileResolution::TakeTheirs)
                )
            })
            .collect()
    }

    /// Submit file resolutions in batch. Mirrors [`Self::resolve`]:
    /// unlike sig resolutions there is no type-check to run (`SetFiles`
    /// carries no program semantics), so this is the only resolve path
    /// files need — no `resolve_files_checked` counterpart.
    pub fn resolve_files(
        &mut self,
        resolutions: Vec<(FilePath, FileResolution)>,
    ) -> Vec<FileResolveVerdict> {
        let mut out = Vec::with_capacity(resolutions.len());
        for (path, resolution) in resolutions {
            if !self.file_conflicts.contains_key(&path) {
                out.push(FileResolveVerdict {
                    path: path.clone(),
                    accepted: false,
                    rejection: Some(FileResolutionRejection::UnknownConflict { path }),
                });
                continue;
            }
            self.file_resolutions.insert(path.clone(), resolution);
            out.push(FileResolveVerdict { path, accepted: true, rejection: None });
        }
        out
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

    /// Submit resolutions in batch, **type-checking each** against the
    /// composed program before accepting it (#834).
    ///
    /// This is the loop the session was built for — "submit N
    /// resolutions, see which broke type-checking, fix them, retry" —
    /// made real. Structural validation ([`Self::validate_resolution`])
    /// runs first; a structurally-valid resolution is then overlaid on
    /// dst's head together with every resolution accepted so far, and
    /// the injected [`ResolutionChecker`] type-checks the result. A
    /// resolution whose composed program doesn't type-check is rejected
    /// with [`ResolutionRejection::TypeError`] and *not* recorded, so
    /// the session's accepted set stays type-correct at every step.
    ///
    /// Resolutions are processed in order and accumulate: a later
    /// resolution is checked against the program the earlier accepted
    /// ones already produced. Interdependent picks (two conflicts that
    /// only compose together) should therefore be submitted in
    /// dependency order, or a rejected one resubmitted after its
    /// partner lands — the same way `git` needs both halves of an
    /// intertwined conflict resolved before the tree builds. Unresolved
    /// conflicts contribute nothing to the projection: they leave dst's
    /// (always-valid) side standing, so a partial batch still composes.
    pub fn resolve_checked(
        &mut self,
        resolutions: Vec<(ConflictId, Resolution)>,
        checker: &dyn ResolutionChecker,
    ) -> Vec<ResolveVerdict> {
        let mut out = Vec::with_capacity(resolutions.len());
        for (conflict_id, resolution) in resolutions {
            // 1. Structural: known conflict, custom op acknowledges
            //    both sides. Cheap, and a malformed op can't be
            //    type-checked meaningfully anyway.
            if let Err(rej) = self.validate_resolution(&conflict_id, &resolution) {
                out.push(ResolveVerdict { conflict_id, accepted: false, rejection: Some(rej) });
                continue;
            }
            // 2. Type: overlay this resolution on the ones accepted so
            //    far and type-check the composed program.
            let mut trial = self.resolutions.clone();
            trial.insert(conflict_id.clone(), resolution.clone());
            let delta = self.projected_delta(&trial);
            let errors = checker.typecheck_projection(&delta);
            if !errors.is_empty() {
                out.push(ResolveVerdict {
                    conflict_id: conflict_id.clone(),
                    accepted: false,
                    rejection: Some(ResolutionRejection::TypeError { conflict_id, errors }),
                });
                continue;
            }
            self.resolutions.insert(conflict_id.clone(), resolution);
            out.push(ResolveVerdict { conflict_id, accepted: true, rejection: None });
        }
        out
    }

    /// The projected post-merge head-delta **against dst's head**,
    /// assuming `resolutions`. This is exactly the `entries` a
    /// `StageTransition::Merge` would record, and the input the
    /// [`ResolutionChecker`] overlays on dst's head:
    ///
    /// * `MergeOutcome::Src` (a change only src made) → set it.
    /// * `MergeOutcome::Both` / `Dst` → dst's head already reflects it;
    ///   no delta.
    /// * conflict resolved `TakeTheirs` → set src's stage.
    /// * conflict resolved `Custom` → set the custom op's target
    ///   ([`OperationKind::merge_target`]).
    /// * conflict resolved `TakeOurs` → dst already has it; no delta.
    /// * conflict unresolved / `Defer` → no delta (dst's side stands).
    fn projected_delta(
        &self,
        resolutions: &BTreeMap<ConflictId, Resolution>,
    ) -> BTreeMap<SigId, Option<StageId>> {
        let mut delta: BTreeMap<SigId, Option<StageId>> = BTreeMap::new();
        for outcome in &self.auto_resolved {
            if let MergeOutcome::Src { sig_id, stage_id } = outcome {
                delta.insert(sig_id.clone(), stage_id.clone());
            }
        }
        for (conflict_id, record) in &self.conflicts {
            match resolutions.get(conflict_id) {
                Some(Resolution::TakeTheirs) => {
                    delta.insert(record.sig_id.clone(), record.theirs.clone());
                }
                Some(Resolution::Custom { op }) => {
                    if let Some((sig, stage)) = op.kind.merge_target() {
                        delta.insert(sig, stage);
                    }
                }
                // TakeOurs (dst already has it), Defer, or unresolved:
                // no change against dst's head.
                _ => {}
            }
        }
        delta
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

    /// Finalize the merge. On success returns the resolved sig and
    /// file resolutions, in id order, plus whether the caller must
    /// append a `SetFiles`. The caller is responsible for synthesizing
    /// the final `Operation::Merge` (and, if `needs_setfiles`, the
    /// follow-up `SetFiles`) op against the store and persisting them;
    /// this function returns the engine's view of "what to land," not
    /// the persisted op ids.
    ///
    /// Sig conflicts are checked before file conflicts: a session with
    /// both kinds pending reports [`CommitError::ConflictsRemaining`]
    /// first, exactly the precedence #977's merge gate already used for
    /// dependency conflicts vs. type errors — one blocker surfaced at a
    /// time keeps the agent's retry loop simple.
    pub fn commit(self) -> Result<MergeCommitOutput, CommitError> {
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
        let unresolved_files: Vec<FilePath> = self
            .file_conflicts
            .keys()
            .filter(|path| {
                !matches!(
                    self.file_resolutions.get(*path),
                    Some(FileResolution::TakeOurs) | Some(FileResolution::TakeTheirs)
                )
            })
            .cloned()
            .collect();
        if !unresolved_files.is_empty() {
            return Err(CommitError::FileConflictsRemaining(unresolved_files));
        }
        let mut resolved: Vec<(ConflictId, Resolution)> = self.resolutions.into_iter().collect();
        resolved.sort_by(|a, b| a.0.cmp(&b.0));
        let mut resolved_files: Vec<(FilePath, FileResolution)> =
            self.file_resolutions.into_iter().collect();
        resolved_files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(MergeCommitOutput { resolved, resolved_files, needs_setfiles: self.needs_setfiles })
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
                    in_file: None,
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
                    to_sig_id: None,
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
                    to_sig_id: None,
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
                    in_file: None,
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
                    in_file: None,
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
                to_sig_id: None,
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
                to_sig_id: None,
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
        let out = session.commit().unwrap();
        assert!(out.resolved.is_empty());
        assert!(out.resolved_files.is_empty());
        assert!(!out.needs_setfiles);
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
            other => panic!("expected ConflictsRemaining, got {other:?}"),
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
            other => panic!("expected ConflictsRemaining, got {other:?}"),
        }
    }

    #[test]
    fn commit_after_resolve_succeeds() {
        let (_tmp, log, dst, src) = fixture();
        let mut session =
            MergeSession::start("ms-12", &log, Some(&src), Some(&dst)).unwrap();
        session.resolve(vec![("fn::A".into(), Resolution::TakeOurs)]);
        let out = session.commit().unwrap();
        assert_eq!(out.resolved.len(), 1);
        assert_eq!(out.resolved[0].0, "fn::A");
        assert!(matches!(out.resolved[0].1, Resolution::TakeOurs));
        assert!(out.resolved_files.is_empty());
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
                    in_file: None,
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

    // ---- #834: resolve_checked type-checks resolutions ----

    /// A `ResolutionChecker` that rejects any projection setting the
    /// conflicted sig to a named "poison" stage — a stand-in for the
    /// real store-backed checker, which composes+type-checks. Records
    /// the deltas it was asked about so tests can assert the
    /// projection shape the session hands the checker.
    struct MockChecker {
        poison_stage: &'static str,
        seen: std::cell::RefCell<Vec<BTreeMap<SigId, Option<StageId>>>>,
    }
    impl MockChecker {
        fn new(poison_stage: &'static str) -> Self {
            Self { poison_stage, seen: std::cell::RefCell::new(Vec::new()) }
        }
    }
    impl ResolutionChecker for MockChecker {
        fn typecheck_projection(&self, delta: &BTreeMap<SigId, Option<StageId>>) -> Vec<String> {
            self.seen.borrow_mut().push(delta.clone());
            if delta.values().any(|s| s.as_deref() == Some(self.poison_stage)) {
                vec![format!("stage {} does not type-check", self.poison_stage)]
            } else {
                Vec::new()
            }
        }
    }

    #[test]
    fn resolve_checked_rejects_a_resolution_that_breaks_typechecking() {
        // theirs == stage-2. A checker that poisons stage-2 must
        // reject TakeTheirs and NOT record it — the session's
        // accepted set stays type-correct.
        let (_tmp, log, dst, src) = fixture();
        let mut session = MergeSession::start("ms-c1", &log, Some(&src), Some(&dst)).unwrap();
        let checker = MockChecker::new("stage-2");

        let verdicts = session.resolve_checked(
            vec![("fn::A".into(), Resolution::TakeTheirs)],
            &checker,
        );
        assert_eq!(verdicts.len(), 1);
        assert!(!verdicts[0].accepted);
        assert!(matches!(
            verdicts[0].rejection,
            Some(ResolutionRejection::TypeError { .. })
        ), "expected TypeError, got {:?}", verdicts[0].rejection);
        // Not recorded → the conflict is still pending.
        assert_eq!(session.remaining_conflicts().len(), 1);
    }

    #[test]
    fn resolve_checked_accepts_a_resolution_that_composes() {
        // TakeOurs keeps stage-1 (dst's side): the projection is
        // empty (dst already has it), so the checker sees no poison
        // and accepts.
        let (_tmp, log, dst, src) = fixture();
        let mut session = MergeSession::start("ms-c2", &log, Some(&src), Some(&dst)).unwrap();
        let checker = MockChecker::new("stage-2");

        let verdicts = session.resolve_checked(
            vec![("fn::A".into(), Resolution::TakeOurs)],
            &checker,
        );
        assert_eq!(verdicts.len(), 1);
        assert!(verdicts[0].accepted, "got {:?}", verdicts[0].rejection);
        assert!(session.remaining_conflicts().is_empty());
        // TakeOurs contributes no delta against dst's head.
        assert_eq!(checker.seen.borrow().last().unwrap().len(), 0);
    }

    #[test]
    fn resolve_checked_still_rejects_structurally_invalid_before_typechecking() {
        // An unknown conflict is rejected structurally; the checker
        // is never consulted for it.
        let (_tmp, log, dst, src) = fixture();
        let mut session = MergeSession::start("ms-c3", &log, Some(&src), Some(&dst)).unwrap();
        let checker = MockChecker::new("stage-2");
        let verdicts = session.resolve_checked(
            vec![("fn::NOPE".into(), Resolution::TakeTheirs)],
            &checker,
        );
        assert!(!verdicts[0].accepted);
        assert!(matches!(
            verdicts[0].rejection,
            Some(ResolutionRejection::UnknownConflict { .. })
        ));
        assert!(checker.seen.borrow().is_empty(), "checker must not run on a structural reject");
    }

    #[test]
    fn projected_delta_sets_theirs_for_take_theirs() {
        let (_tmp, log, dst, src) = fixture();
        let session = MergeSession::start("ms-c4", &log, Some(&src), Some(&dst)).unwrap();
        let mut res = BTreeMap::new();
        res.insert("fn::A".to_string(), Resolution::TakeTheirs);
        let delta = session.projected_delta(&res);
        assert_eq!(delta.get("fn::A"), Some(&Some("stage-2".to_string())));
    }

    // ---- #1007 PR 7: file conflicts on a merge session ----

    fn some_entry(n: u8) -> FileEntry {
        FileEntry { blob: format!("{n:0>64}"), mode: "100644".into(), size: n as u64 }
    }

    fn file_conflict(path: &str) -> FileConflict {
        FileConflict {
            path: path.into(),
            base: Some(some_entry(1)),
            ours: Some(some_entry(2)),
            theirs: Some(some_entry(3)),
        }
    }

    #[test]
    fn no_file_conflicts_by_default() {
        // A session that never gets `attach_file_conflicts` called on
        // it (every merge before #1007, and any merge whose manifests
        // already agree) has no files dimension at all.
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let session = MergeSession::start("ms-f0", &log, None, None).unwrap();
        assert!(session.remaining_file_conflicts().is_empty());
        assert!(!session.needs_setfiles());
        let out = session.commit().unwrap();
        assert!(out.resolved_files.is_empty());
        assert!(!out.needs_setfiles);
    }

    #[test]
    fn attach_file_conflicts_surfaces_them_as_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let mut session = MergeSession::start("ms-f1", &log, None, None).unwrap();
        session.attach_file_conflicts(vec![file_conflict("README.md")], true);
        assert!(session.needs_setfiles());
        let remaining = session.remaining_file_conflicts();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].path, "README.md");
    }

    #[test]
    fn commit_blocked_by_unresolved_file_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let mut session = MergeSession::start("ms-f2", &log, None, None).unwrap();
        session.attach_file_conflicts(vec![file_conflict("README.md")], true);
        let err = session.commit().unwrap_err();
        match err {
            CommitError::FileConflictsRemaining(paths) => {
                assert_eq!(paths, vec!["README.md".to_string()]);
            }
            other => panic!("expected FileConflictsRemaining, got {other:?}"),
        }
    }

    #[test]
    fn sig_conflicts_take_precedence_over_file_conflicts_in_commit_error() {
        // A session with BOTH an unresolved sig conflict and an
        // unresolved file conflict reports the sig one first — the
        // agent fixes one blocker at a time.
        let (_tmp, log, dst, src) = fixture();
        let mut session = MergeSession::start("ms-f3", &log, Some(&src), Some(&dst)).unwrap();
        session.attach_file_conflicts(vec![file_conflict("README.md")], true);
        let err = session.commit().unwrap_err();
        assert!(matches!(err, CommitError::ConflictsRemaining(_)));
    }

    #[test]
    fn resolve_files_take_ours_clears_conflict_and_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let mut session = MergeSession::start("ms-f4", &log, None, None).unwrap();
        session.attach_file_conflicts(vec![file_conflict("README.md")], true);
        let verdicts =
            session.resolve_files(vec![("README.md".into(), FileResolution::TakeOurs)]);
        assert!(verdicts[0].accepted);
        assert!(session.remaining_file_conflicts().is_empty());
        let out = session.commit().unwrap();
        assert_eq!(out.resolved_files, vec![("README.md".to_string(), FileResolution::TakeOurs)]);
        assert!(out.needs_setfiles);
    }

    #[test]
    fn resolve_files_unknown_path_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let mut session = MergeSession::start("ms-f5", &log, None, None).unwrap();
        session.attach_file_conflicts(vec![file_conflict("README.md")], true);
        let verdicts =
            session.resolve_files(vec![("nope.txt".into(), FileResolution::TakeOurs)]);
        assert!(!verdicts[0].accepted);
        assert!(matches!(
            verdicts[0].rejection,
            Some(FileResolutionRejection::UnknownConflict { .. })
        ));
        // The real conflict is untouched.
        assert_eq!(session.remaining_file_conflicts().len(), 1);
    }

    #[test]
    fn defer_on_file_conflict_keeps_it_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let log = OpLog::open(tmp.path()).unwrap();
        let mut session = MergeSession::start("ms-f6", &log, None, None).unwrap();
        session.attach_file_conflicts(vec![file_conflict("README.md")], true);
        let verdicts =
            session.resolve_files(vec![("README.md".into(), FileResolution::Defer)]);
        assert!(verdicts[0].accepted);
        assert_eq!(session.remaining_file_conflicts().len(), 1, "defer is not a resolution");
        assert!(matches!(
            session.commit().unwrap_err(),
            CommitError::FileConflictsRemaining(_)
        ));
    }
}
