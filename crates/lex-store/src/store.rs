//! `Store` — content-addressed code repository.
//!
//! The filesystem is the source of truth. All operations read/write JSON
//! files under `<root>/stages/<SigId>/`. There is no SQLite cache: every
//! query walks the directory and parses what's needed. `cargo test`
//! runs aren't perf-critical and the §4.6 acceptance requires the
//! rebuild-from-filesystem property anyway.

use crate::branches::DEFAULT_BRANCH;
use crate::model::*;
use lex_ast::{sig_id, stage_id, Stage};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("imports cannot be published as stages")]
    CannotPublishImport,
    #[error("unknown stage_id `{0}`")]
    UnknownStage(String),
    #[error("unknown sig_id `{0}`")]
    UnknownSig(String),
    #[error("invalid lifecycle transition: {0}")]
    InvalidTransition(String),
    #[error("unknown branch `{0}`")]
    UnknownBranch(String),
    #[error("unknown op_id `{0}`")]
    UnknownOp(lex_vcs::OpId),
    /// A typed AST transform (#280) — e.g. `ReplaceMatchArm` — was
    /// asked to operate on a node it couldn't address (wrong kind,
    /// out-of-range arm index, unknown NodeId, etc.). Distinct from
    /// `TypeError` (which means the transform succeeded but its
    /// output didn't typecheck) so callers can render the right
    /// error message.
    #[error("transform failed: {0}")]
    TransformError(lex_ast::TransformError),
    #[error(transparent)]
    Apply(#[from] lex_vcs::ApplyError),
    /// The candidate program — i.e. the source the caller is
    /// publishing — doesn't typecheck. The branch head is unchanged
    /// and no op records are persisted. Issue #130's "always-valid
    /// HEAD" invariant: the gate runs before any side effect, so a
    /// type-broken publish leaves no footprint.
    #[error("type errors in published program: {} error(s)", .0.len())]
    TypeError(Vec<lex_types::TypeError>),
    /// The op was persisted but a `required_attestations` rule in
    /// `policy.json` (#245) refused to advance the branch head past
    /// it. The op record is durable — re-running with the missing
    /// attestations recorded will succeed without re-persisting —
    /// but the branch is unchanged. Surfaced as a structured JSON
    /// envelope on the HTTP API.
    #[error(
        "branch advance blocked: op {} missing attestations: {}",
        .0.op_id, .0.missing.join(", ")
    )]
    BranchAdvanceBlocked(crate::policy::BranchAdvanceBlocked),
    /// All retry attempts of the CAS branch-head advance failed
    /// because another writer kept advancing the same branch
    /// (#262). The op record itself is durable in the op log
    /// (orphaned), so re-running with backoff would eventually
    /// land — return `503 Contention { retry_after }` from the
    /// HTTP API and let the client back off.
    #[error("branch advance contention on `{branch}`: {attempts} retries exhausted")]
    Contention { branch: String, attempts: u32 },
    /// The op was persisted but its stage carries an attestation
    /// produced by a retroactively quarantined tool (#248). The
    /// branch head is unchanged. The op record stays in the log
    /// (audit trail intact); re-running with the producer
    /// unblocked, or with un-contaminated attestations, succeeds
    /// without re-persisting the op.
    #[error(
        "branch advance blocked: op {} touches stage {} with an attestation from \
         quarantined producer `{}` (blocked at {}, attestation at {})",
        .0.op_id, .0.stage_id, .0.tool_id, .0.blocked_at, .0.attestation_at
    )]
    ProducerBlocked(crate::policy::ProducerBlocked),
    /// The op would push its session's monotonic budget over the
    /// cap configured in `policy.session_budgets` (#292 slice 3).
    /// The op is *not* persisted; the branch head is unchanged.
    /// The caller should either start a new session, raise the
    /// cap, or refactor to fit the budget. HTTP API maps to 503.
    #[error(
        "session `{session_id}` budget exceeded: spent_after={spent_after} > cap={cap}"
    )]
    BudgetExceeded {
        session_id: String,
        cap: u64,
        spent_after: u64,
    },
}

/// The outcome returned by [`Store::publish_program`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublishOutcome {
    pub ops: Vec<PublishOp>,
    pub head_op: Option<lex_vcs::OpId>,
}

/// One applied operation within a [`PublishOutcome`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublishOp {
    pub op_id: lex_vcs::OpId,
    pub kind: serde_json::Value,
}

/// One entry in the per-`SigId` stage history surfaced by
/// `Store::sig_history`. Newest-first ordering is the responsibility
/// of the producer.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct StageHistoryEntry {
    pub stage_id: String,
    pub status: StageStatus,
    /// Wall-clock seconds of the most recent transition.
    pub last_at: u64,
    /// Wall-clock seconds when this stage was first written to the
    /// store (its initial Draft transition). `None` for stages
    /// whose lifecycle log doesn't include an explicit Draft entry
    /// — shouldn't happen for stages published via `Store::publish`,
    /// but the type allows hand-edited stores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<u64>,
}

/// Per-candidate metadata surfaced by [`Store::list_candidates`]
/// (#294). Returned sorted by `op_id` for deterministic output.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CandidateInfo {
    pub op_id: lex_vcs::OpId,
    pub stage_id: lex_vcs::StageId,
    /// Author intent. Always set for `Candidate` ops emitted via
    /// [`Store::propose_candidate`]; `None` only if a
    /// hand-written raw op skipped the intent tag.
    pub intent_id: Option<lex_vcs::IntentId>,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open or create a store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("stages"))?;
        fs::create_dir_all(root.join("traces"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path { &self.root }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }

    fn sig_dir(&self, sig: &str) -> PathBuf { self.root.join("stages").join(sig) }
    fn impl_dir(&self, sig: &str) -> PathBuf { self.sig_dir(sig).join("implementations") }
    fn tests_dir(&self, sig: &str) -> PathBuf { self.sig_dir(sig).join("tests") }
    fn specs_dir(&self, sig: &str) -> PathBuf { self.sig_dir(sig).join("specs") }
    fn lifecycle_path(&self, sig: &str) -> PathBuf { self.sig_dir(sig).join("lifecycle.json") }

    // ---- publish ----

    /// Publish a stage as **Draft**. Returns the StageId.
    /// Idempotent: republishing the same canonical AST returns the same
    /// StageId without writing duplicates.
    pub fn publish(&self, stage: &Stage) -> Result<String, StoreError> {
        self.publish_signed(stage, None)
    }

    /// Like [`Self::publish`] but optionally attaches an Ed25519
    /// signature over the StageId (#227). When `signer` is `Some`,
    /// the persisted metadata gets a `signature` field that
    /// downstream consumers can verify via
    /// [`lex_vcs::verify_stage_id`].
    ///
    /// Idempotency: if a metadata file already exists the signature
    /// is *not* re-written. This preserves "republishing is a no-op"
    /// even across different signers — promoting a signed stage
    /// requires a fresh stage hash anyway, so a metadata overwrite
    /// would be the wrong primitive.
    pub fn publish_signed(
        &self,
        stage: &Stage,
        signer: Option<&lex_vcs::Keypair>,
    ) -> Result<String, StoreError> {
        let sig = sig_id(stage).ok_or(StoreError::CannotPublishImport)?;
        let stage_id = stage_id(stage).ok_or(StoreError::CannotPublishImport)?;
        let name = stage_name(stage).to_string();

        fs::create_dir_all(self.impl_dir(&sig))?;
        fs::create_dir_all(self.tests_dir(&sig))?;
        fs::create_dir_all(self.specs_dir(&sig))?;

        let ast_path = self.impl_dir(&sig).join(format!("{}.ast.json", stage_id));
        let delta_path = self.impl_dir(&sig).join(format!("{}.delta.json", stage_id));
        let meta_path = self.impl_dir(&sig).join(format!("{}.metadata.json", stage_id));

        // #261 slice 3: try delta encoding against the most recent
        // prior stage in this sig's lifecycle. Falls back to a full
        // snapshot when (a) no prior stage exists, (b) the diff
        // ratio is over the threshold, or (c) the delta chain is
        // already at its cap. The decision is internal — callers
        // see the same `Stage` object on `get_ast` regardless.
        if !ast_path.exists() && !delta_path.exists() {
            self.persist_stage_bytes(&sig, &stage_id, stage, &ast_path, &delta_path)?;
        }
        if !meta_path.exists() {
            let signature = signer.map(|kp| kp.sign_stage_id(&stage_id));
            let metadata = Metadata {
                stage_id: stage_id.clone(),
                sig_id: sig.clone(),
                name,
                published_at: Self::now(),
                note: None,
                signature,
            };
            write_canonical_json(&meta_path, &metadata)?;
        }

        // Lifecycle: append a Draft transition for first publish.
        let mut life = self.read_lifecycle(&sig).unwrap_or_else(|_| Lifecycle {
            sig_id: sig.clone(),
            ..Default::default()
        });
        if !life.transitions.iter().any(|t| t.stage_id == stage_id) {
            life.transitions.push(Transition {
                stage_id: stage_id.clone(),
                from: StageStatus::Draft, // synthesized; "from" of first transition is itself
                to: StageStatus::Draft,
                at: Self::now(),
                reason: None,
            });
            self.write_lifecycle(&sig, &life)?;
        }
        Ok(stage_id)
    }

    // ---- lifecycle ----

    pub fn activate(&self, stage_id: &str) -> Result<(), StoreError> {
        let (sig, mut life) = self.lookup_lifecycle(stage_id)?;
        // Demote any currently-Active impls for this SigId to Deprecated.
        let active = life.current_active().map(|s| s.to_string());
        if let Some(prev) = active {
            if prev != stage_id {
                life.transitions.push(Transition {
                    stage_id: prev,
                    from: StageStatus::Active,
                    to: StageStatus::Deprecated,
                    at: Self::now(),
                    reason: Some("superseded".into()),
                });
            }
        }
        let cur = life.status_of(stage_id);
        if cur == Some(StageStatus::Tombstone) {
            return Err(StoreError::InvalidTransition("tombstoned cannot be activated".into()));
        }
        life.transitions.push(Transition {
            stage_id: stage_id.into(),
            from: cur.unwrap_or(StageStatus::Draft),
            to: StageStatus::Active,
            at: Self::now(),
            reason: None,
        });
        self.write_lifecycle(&sig, &life)
    }

    pub fn deprecate(&self, stage_id: &str, reason: impl Into<String>) -> Result<(), StoreError> {
        let (sig, mut life) = self.lookup_lifecycle(stage_id)?;
        let cur = life.status_of(stage_id).ok_or_else(|| StoreError::UnknownStage(stage_id.into()))?;
        if cur != StageStatus::Active {
            return Err(StoreError::InvalidTransition(format!("{cur:?} ⇒ Deprecated")));
        }
        life.transitions.push(Transition {
            stage_id: stage_id.into(),
            from: cur,
            to: StageStatus::Deprecated,
            at: Self::now(),
            reason: Some(reason.into()),
        });
        self.write_lifecycle(&sig, &life)
    }

    pub fn tombstone(&self, stage_id: &str) -> Result<(), StoreError> {
        let (sig, mut life) = self.lookup_lifecycle(stage_id)?;
        let cur = life.status_of(stage_id).ok_or_else(|| StoreError::UnknownStage(stage_id.into()))?;
        if cur != StageStatus::Deprecated {
            return Err(StoreError::InvalidTransition(format!("{cur:?} ⇒ Tombstone")));
        }
        life.transitions.push(Transition {
            stage_id: stage_id.into(),
            from: cur,
            to: StageStatus::Tombstone,
            at: Self::now(),
            reason: None,
        });
        self.write_lifecycle(&sig, &life)
    }

    // ---- queries ----

    /// The current Active StageId for a signature, or `None`.
    pub fn resolve_sig(&self, sig: &str) -> Result<Option<String>, StoreError> {
        let life = match self.read_lifecycle(sig) {
            Ok(l) => l,
            Err(_) => return Ok(None),
        };
        Ok(life.current_active().map(|s| s.to_string()))
    }

    /// Per-stage history for a SigId, ordered chronologically by
    /// the *last* transition timestamp. Returns one entry per
    /// distinct StageId that has ever been published under `sig`.
    /// `Ok(vec![])` if the SigId doesn't exist in the store.
    ///
    /// Used by `lex blame` to render "where does this fn come from".
    pub fn sig_history(&self, sig: &str) -> Result<Vec<StageHistoryEntry>, StoreError> {
        let life = match self.read_lifecycle(sig) {
            Ok(l) => l,
            Err(_) => return Ok(Vec::new()),
        };
        // Collapse transitions: latest status + last_at per stage,
        // plus the timestamp of the first Draft transition (≈ when
        // the stage was published) when one exists.
        let mut by_stage: indexmap::IndexMap<String, StageHistoryEntry> =
            indexmap::IndexMap::new();
        for t in &life.transitions {
            let entry = by_stage.entry(t.stage_id.clone()).or_insert(StageHistoryEntry {
                stage_id: t.stage_id.clone(),
                status: t.to,
                last_at: t.at,
                published_at: None,
            });
            entry.status = t.to;
            entry.last_at = t.at;
            if t.from == StageStatus::Draft && entry.published_at.is_none() {
                entry.published_at = Some(t.at);
            }
            if t.to == StageStatus::Draft && entry.published_at.is_none() {
                // Initial publication: Draft is the *destination*.
                entry.published_at = Some(t.at);
            }
        }
        let mut out: Vec<StageHistoryEntry> = by_stage.into_values().collect();
        // Sort newest first so `lex blame` shows recent activity at top.
        out.sort_by_key(|e| std::cmp::Reverse(e.last_at));
        Ok(out)
    }

    pub fn get_ast(&self, stage_id: &str) -> Result<Stage, StoreError> {
        let (sig, _) = self.lookup_lifecycle(stage_id)?;
        let bytes = self.read_stage_canonical_bytes(&sig, stage_id)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Read the canonical bytes of a stage, walking back through
    /// any delta chain (#261 slice 3). The recursion ends at a
    /// `<stage_id>.ast.json` file (a full snapshot) or, in the
    /// degenerate case of a missing chain, with `UnknownStage`.
    fn read_stage_canonical_bytes(
        &self,
        sig: &str,
        stage_id: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let ast_path = self.impl_dir(sig).join(format!("{}.ast.json", stage_id));
        if ast_path.exists() {
            return Ok(fs::read(&ast_path)?);
        }
        let delta_path = self.impl_dir(sig).join(format!("{}.delta.json", stage_id));
        if !delta_path.exists() {
            return Err(StoreError::UnknownStage(stage_id.into()));
        }
        let delta_bytes = fs::read(&delta_path)?;
        let delta: crate::delta::StageDelta = serde_json::from_slice(&delta_bytes)?;
        let base_bytes = self.read_stage_canonical_bytes(sig, &delta.base_stage_id)?;
        crate::delta::apply(&base_bytes, &delta)
            .map_err(|e| StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("applying delta for {stage_id}: {e}"),
            )))
    }

    /// Persist a freshly-published stage's canonical bytes (#261
    /// slice 3). Tries delta encoding against the most recent
    /// prior stage in the sig's lifecycle; falls back to a full
    /// snapshot when no base exists, the diff ratio is too high,
    /// or the delta chain is already at its cap.
    fn persist_stage_bytes(
        &self,
        sig: &str,
        stage_id: &str,
        stage: &Stage,
        ast_path: &Path,
        delta_path: &Path,
    ) -> Result<(), StoreError> {
        let new_bytes = canonical_bytes(stage)?;
        if let Some((base_stage_id, base_chain_length)) =
            self.pick_delta_base(sig, stage_id)?
        {
            let base_bytes = self.read_stage_canonical_bytes(sig, &base_stage_id)?;
            let (prefix, suffix, middle) = crate::delta::splice(&base_bytes, &new_bytes);
            let chain_length = base_chain_length + 1;
            if crate::delta::is_worth_encoding(middle.len(), new_bytes.len(), chain_length) {
                let delta = crate::delta::StageDelta {
                    base_stage_id,
                    chain_length,
                    common_prefix: prefix,
                    common_suffix: suffix,
                    middle_hex: hex::encode(&middle),
                };
                write_canonical_json(delta_path, &delta)?;
                return Ok(());
            }
        }
        // Fall through: full snapshot.
        if let Some(parent) = ast_path.parent() { fs::create_dir_all(parent)?; }
        fs::write(ast_path, &new_bytes)?;
        Ok(())
    }

    /// Pick a base stage for delta encoding from the given sig's
    /// lifecycle. Returns `(base_stage_id, base_chain_length)` for
    /// the most-recent non-tombstoned prior stage, or `None` when
    /// there is no candidate. The chain length is read off the
    /// base's `.delta.json` (if any) to enforce the cap.
    fn pick_delta_base(
        &self,
        sig: &str,
        new_stage_id: &str,
    ) -> Result<Option<(String, usize)>, StoreError> {
        let life = self.read_lifecycle(sig).ok();
        let Some(life) = life else { return Ok(None); };
        // Walk transitions newest-first; pick the first prior
        // stage that isn't this one and isn't tombstoned.
        let mut latest_per_stage: indexmap::IndexMap<&str, StageStatus> = indexmap::IndexMap::new();
        for t in &life.transitions {
            latest_per_stage.insert(&t.stage_id, t.to);
        }
        let mut candidates: Vec<&str> = latest_per_stage
            .iter()
            .filter(|(id, status)| {
                **id != new_stage_id && **status != StageStatus::Tombstone
            })
            .map(|(id, _)| *id)
            .collect();
        // Reverse to get newest-first (transitions are append-only,
        // so latest_per_stage's iteration order matches insertion
        // order, oldest-first).
        candidates.reverse();
        let Some(&base) = candidates.first() else { return Ok(None); };
        let base_chain_length = self.delta_chain_length(sig, base)?;
        Ok(Some((base.to_string(), base_chain_length)))
    }

    /// Length of the delta chain ending at `stage_id`. Zero when
    /// the stage is a full snapshot (`.ast.json` present); the
    /// stored `chain_length` from `.delta.json` otherwise.
    fn delta_chain_length(&self, sig: &str, stage_id: &str) -> Result<usize, StoreError> {
        let ast_path = self.impl_dir(sig).join(format!("{}.ast.json", stage_id));
        if ast_path.exists() {
            return Ok(0);
        }
        let delta_path = self.impl_dir(sig).join(format!("{}.delta.json", stage_id));
        if !delta_path.exists() {
            return Ok(0);
        }
        let bytes = fs::read(&delta_path)?;
        let delta: crate::delta::StageDelta = serde_json::from_slice(&bytes)?;
        Ok(delta.chain_length)
    }

    pub fn get_metadata(&self, stage_id: &str) -> Result<Metadata, StoreError> {
        let (sig, _) = self.lookup_lifecycle(stage_id)?;
        let path = self.impl_dir(&sig).join(format!("{}.metadata.json", stage_id));
        let bytes = fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn get_status(&self, stage_id: &str) -> Result<StageStatus, StoreError> {
        let (_sig, life) = self.lookup_lifecycle(stage_id)?;
        life.status_of(stage_id).ok_or_else(|| StoreError::UnknownStage(stage_id.into()))
    }

    pub fn list_stages_by_name(&self, name: &str) -> Result<Vec<String>, StoreError> {
        // Walk every SigId → check metadata of any implementation; if its
        // name matches, include the SigId.
        let mut out = Vec::new();
        let stages_dir = self.root.join("stages");
        if !stages_dir.exists() { return Ok(out); }
        for entry in fs::read_dir(&stages_dir)? {
            let entry = entry?;
            let sig_dir = entry.path();
            if !sig_dir.is_dir() { continue; }
            let sig = entry.file_name().to_string_lossy().to_string();
            // Look at any one metadata file under this SigId.
            let impls = self.impl_dir(&sig);
            if !impls.exists() { continue; }
            for f in fs::read_dir(impls)? {
                let f = f?;
                let p = f.path();
                if p.extension().is_some_and(|e| e == "json")
                    && p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".metadata.json"))
                {
                    if let Ok(bytes) = fs::read(&p) {
                        if let Ok(m) = serde_json::from_slice::<Metadata>(&bytes) {
                            if m.name == name {
                                if !out.contains(&sig) { out.push(sig.clone()); }
                                break;
                            }
                        }
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn list_sigs(&self) -> Result<Vec<String>, StoreError> {
        let stages_dir = self.root.join("stages");
        let mut out = Vec::new();
        if !stages_dir.exists() { return Ok(out); }
        for entry in fs::read_dir(stages_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                out.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    // ---- tests/specs as metadata (§4.4) ----

    pub fn attach_test(&self, sig: &str, test: &Test) -> Result<String, StoreError> {
        if !self.sig_dir(sig).exists() {
            return Err(StoreError::UnknownSig(sig.into()));
        }
        fs::create_dir_all(self.tests_dir(sig))?;
        let path = self.tests_dir(sig).join(format!("{}.json", test.id));
        write_canonical_json(&path, test)?;
        Ok(test.id.clone())
    }

    pub fn list_tests(&self, sig: &str) -> Result<Vec<Test>, StoreError> {
        let dir = self.tests_dir(sig);
        if !dir.exists() { return Ok(Vec::new()); }
        let mut out = Vec::new();
        for f in fs::read_dir(dir)? {
            let f = f?;
            if f.path().extension().is_some_and(|e| e == "json") {
                let bytes = fs::read(f.path())?;
                out.push(serde_json::from_slice(&bytes)?);
            }
        }
        Ok(out)
    }

    pub fn attach_spec(&self, sig: &str, spec: &Spec) -> Result<String, StoreError> {
        if !self.sig_dir(sig).exists() {
            return Err(StoreError::UnknownSig(sig.into()));
        }
        fs::create_dir_all(self.specs_dir(sig))?;
        let path = self.specs_dir(sig).join(format!("{}.json", spec.id));
        write_canonical_json(&path, spec)?;
        Ok(spec.id.clone())
    }

    pub fn list_specs(&self, sig: &str) -> Result<Vec<Spec>, StoreError> {
        let dir = self.specs_dir(sig);
        if !dir.exists() { return Ok(Vec::new()); }
        let mut out = Vec::new();
        for f in fs::read_dir(dir)? {
            let f = f?;
            if f.path().extension().is_some_and(|e| e == "json") {
                let bytes = fs::read(f.path())?;
                out.push(serde_json::from_slice(&bytes)?);
            }
        }
        Ok(out)
    }

    // ---- traces (§4.2 / M7) ----

    fn trace_path(&self, run_id: &str) -> PathBuf {
        self.root.join("traces").join(run_id).join("trace.json")
    }

    pub fn save_trace(&self, tree: &lex_trace::TraceTree) -> Result<String, StoreError> {
        let path = self.trace_path(&tree.run_id);
        write_canonical_json(&path, tree)?;
        Ok(tree.run_id.clone())
    }

    pub fn load_trace(&self, run_id: &str) -> Result<lex_trace::TraceTree, StoreError> {
        let bytes = fs::read(self.trace_path(run_id))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn list_traces(&self) -> Result<Vec<String>, StoreError> {
        let dir = self.root.join("traces");
        if !dir.exists() { return Ok(Vec::new()); }
        let mut out = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                out.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    // ---- internals ----

    fn lookup_lifecycle(&self, stage_id: &str) -> Result<(String, Lifecycle), StoreError> {
        // Walk every SigId, find which one contains this StageId.
        for sig in self.list_sigs()? {
            if let Ok(life) = self.read_lifecycle(&sig) {
                if life.transitions.iter().any(|t| t.stage_id == stage_id) {
                    return Ok((sig, life));
                }
            }
        }
        Err(StoreError::UnknownStage(stage_id.into()))
    }

    fn read_lifecycle(&self, sig: &str) -> Result<Lifecycle, StoreError> {
        let path = self.lifecycle_path(sig);
        if !path.exists() {
            return Ok(Lifecycle { sig_id: sig.into(), transitions: Vec::new() });
        }
        let bytes = fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn write_lifecycle(&self, sig: &str, life: &Lifecycle) -> Result<(), StoreError> {
        write_canonical_json(&self.lifecycle_path(sig), life)
    }

    /// Apply a published program to a branch as a sequence of typed
    /// operations. Returns the ordered list of op_ids + the new
    /// head_op. The caller (`lex publish` CLI, `lex serve`'s HTTP
    /// handler) is responsible for computing the `DiffReport` against
    /// the current branch head — the diff infrastructure lives in
    /// `lex-vcs::compute_diff` (previously `lex-cli`) to keep this
    /// layer from owning diffing logic.
    ///
    /// On success: every op in the returned list is durable in the
    /// op log and the branch's head_op points at the last one.
    /// On a no-op (no diff): returns empty `ops` and the existing
    /// `head_op` unchanged.
    pub fn publish_program(
        &self,
        branch: &str,
        stages: &[lex_ast::Stage],
        diff: &lex_vcs::DiffReport,
        new_imports: &lex_vcs::ImportMap,
        activate: bool,
    ) -> Result<PublishOutcome, StoreError> {
        self.publish_program_signed(branch, stages, diff, new_imports, activate, None)
    }

    /// Signed variant of [`Self::publish_program`] (#227). Every
    /// stage written under this batch gets the same signer; per-stage
    /// keys aren't supported because the agent identity model treats
    /// a publish as a single authorial act.
    pub fn publish_program_signed(
        &self,
        branch: &str,
        stages: &[lex_ast::Stage],
        diff: &lex_vcs::DiffReport,
        new_imports: &lex_vcs::ImportMap,
        activate: bool,
        signer: Option<&lex_vcs::Keypair>,
    ) -> Result<PublishOutcome, StoreError> {
        use std::collections::{BTreeMap, BTreeSet};

        // #130's write-time gate: verify the candidate program
        // typechecks (and effects are correctly declared) before
        // any disk side-effect. If anything fails, return the
        // structured envelope and leave the branch head unchanged
        // — the store's "always-valid HEAD" invariant only holds
        // because this is the only batch-publish path that
        // advances heads. Single-op writes via the lower-level
        // `apply_operation` are not gated yet (#130 follow-up).
        if let Err(errors) = lex_types::check_program(stages) {
            return Err(StoreError::TypeError(errors));
        }

        // Build old-side views from the current branch.
        let old_head = self.branch_head(branch)?;
        let old_name_to_sig: BTreeMap<String, String> = old_head.iter()
            .filter_map(|(sig, stg)| {
                self.get_metadata(stg).ok().map(|m| (m.name, sig.clone()))
            })
            .collect();
        let old_effects: BTreeMap<String, BTreeSet<String>> = old_head.iter()
            .filter_map(|(sig, stg)| {
                let ast = self.get_ast(stg).ok()?;
                match ast {
                    lex_ast::Stage::FnDecl(fd) => {
                        let s: BTreeSet<String> = fd.effects.iter()
                            .map(|e| e.name.clone()).collect();
                        Some((sig.clone(), s))
                    }
                    _ => None,
                }
            })
            .collect();
        let old_imports = self.derive_imports_from_oplog(branch)?;

        let op_kinds = lex_vcs::diff_to_ops(lex_vcs::DiffInputs {
            old_head: &old_head,
            old_name_to_sig: &old_name_to_sig,
            old_effects: &old_effects,
            old_imports: &old_imports,
            new_stages: stages,
            new_imports,
            diff,
        }).map_err(|e| StoreError::InvalidTransition(format!("diff_to_ops: {e}")))?;

        let mut ops_out: Vec<PublishOp> = Vec::new();
        let mut last_op_id: Option<lex_vcs::OpId> = None;
        for kind in op_kinds {
            // Persist the underlying stage AST/metadata if this op
            // produces or replaces one.
            if let Some(stg) = stage_for_kind(&kind, stages) {
                if !matches!(stg, lex_ast::Stage::Import(_)) {
                    self.publish_signed(stg, signer)?;
                    if activate {
                        if let Some(stage_id_str) = stage_id(stg) {
                            let _ = self.activate(&stage_id_str);
                        }
                    }
                }
            }
            let transition = transition_for_kind(&kind);
            let attestable = attestable_stage_ids(&transition);
            let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
            let op = lex_vcs::Operation::new(
                kind.clone(),
                head_now.into_iter().collect::<Vec<_>>(),
            );
            let op_id = self.apply_operation(branch, op, transition)?;
            self.record_typecheck_passed(&attestable, &op_id)?;
            ops_out.push(PublishOp {
                op_id: op_id.clone(),
                kind: serde_json::to_value(&kind)
                    .map_err(StoreError::Serde)?,
            });
            last_op_id = Some(op_id);
        }

        let head_op = match last_op_id {
            Some(id) => Some(id),
            // No ops applied; return whatever the head was already.
            None => self.get_branch(branch)?.and_then(|b| b.head_op),
        };

        Ok(PublishOutcome {
            ops: ops_out,
            head_op,
        })
    }

    pub fn derive_imports_from_oplog(
        &self,
        branch: &str,
    ) -> Result<lex_vcs::ImportMap, StoreError> {
        use lex_vcs::OperationKind::*;
        let log = lex_vcs::OpLog::open(self.root())?;
        let head = match self.get_branch(branch)?.and_then(|b| b.head_op) {
            Some(h) => h,
            None => return Ok(Default::default()),
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

    /// Apply an operation to a branch and advance its head_op.
    ///
    /// The single advance path. Validates parents via `lex_vcs::apply`,
    /// persists the operation via the op log, then atomically advances
    /// the branch file's head_op via `set_branch_head_op`.
    ///
    /// Errors:
    /// - `UnknownBranch`: branch does not exist (no op is persisted).
    /// - `Apply(ApplyError::StaleParent)`: the op's parents don't
    ///   match the branch head — head is unchanged. Callers that
    ///   want retry-on-stale (e.g. `lex publish` re-running against
    ///   a moved head) match on this variant explicitly.
    /// - `Apply(ApplyError::UnknownMergeParent)`: a merge op's
    ///   second parent isn't in the log.
    /// - `Io`: filesystem error during persist or branch advance.
    ///
    /// Crash recovery: between op persist and branch advance, a crash
    /// can leave an orphan op record in the log with no branch
    /// pointing at it. The op is content-addressed and cheap to
    /// re-derive from the same source. See
    /// Apply a single op against `branch`, gated on the candidate
    /// program typechecking. The per-op variant of #130's
    /// write-time gate — counterpart to [`Self::publish_program`]'s
    /// batch-mode check.
    ///
    /// `candidate` is the sequence of `Stage`s that *would* exist
    /// on this branch after the op is applied. Caller's
    /// responsibility: today neither `lex-store` nor `lex-vcs`
    /// reconstruct the candidate from the op + branch state on
    /// behalf of the caller. The natural callers (HTTP `POST
    /// /v1/publish` for a single op; agent harnesses driving
    /// merges via the future #134 API) already have the candidate
    /// in memory.
    ///
    /// On rejection: branch head unchanged, no op record persisted.
    /// Same atomicity guarantee as the publish path.
    ///
    /// # Why a separate method, not a flag on `apply_operation`
    ///
    /// The merge engine in `lex-vcs::merge` calls
    /// `Store::apply_operation` directly to land merge ops, and at
    /// merge time the resolved program isn't a single `Vec<Stage>`
    /// the way it is on the publish path — it's a per-sig
    /// resolution map. Forcing a candidate through `apply_operation`
    /// would either require the merge engine to assemble one (slow,
    /// every active stage off disk) or accept `Option<&[Stage]>`
    /// and silently skip the gate — the second is exactly the kind
    /// of "secretly opt-out" path #130 is trying to remove. The
    /// honest split is two methods: `apply_operation` for callers
    /// that already typecheck their inputs (or don't need to —
    /// rare, but the merge-resolve case), `apply_operation_checked`
    /// for everyone else.
    pub fn apply_operation_checked(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
        candidate: &[lex_ast::Stage],
    ) -> Result<lex_vcs::OpId, StoreError> {
        if let Err(errors) = lex_types::check_program(candidate) {
            // #281: emit a `RepairHint` attestation against each
            // candidate stage the transition was about to produce.
            // The op record itself isn't persisted (the gate is
            // pre-persistence), but the candidate stage IS — the
            // transform-flow methods publish before this call.
            // The attached hint lets `lex repair <op_id>` and
            // future LLM-assisted apply paths read the structured
            // errors without re-running the typecheck.
            let attestable = attestable_stage_ids(&transition);
            let failed_op_id = op.op_id();
            let _ = self.record_repair_hint(&attestable, &failed_op_id, &errors);
            return Err(StoreError::TypeError(errors));
        }
        // #292 slice 3: per-session budget gate. After typecheck
        // passes, refuse the op if it would push its session's
        // monotonic spend over the configured cap. Sessions
        // without an intent_id, or with an intent whose session
        // has no cap configured, sail through.
        self.check_session_budget(&op)?;
        let attestable = attestable_stage_ids(&transition);
        let op_effects = op_declared_effects(&op.kind);
        // #262: CAS retry loop. Single-parent ops can be safely
        // re-persisted under a new parent on contention (the kind
        // is invariant; only `parents` changes). Merge ops (already
        // 2-parent) come through the merge engine which has its own
        // coordination; we don't retry them here — we'll see the
        // first attempt's CAS fail and surface Contention.
        self.cas_retry_advance(branch, op, transition, |new_head| {
            self.record_typecheck_passed(&attestable, &new_head.op_id)?;
            self.run_required_attestations_gate(
                branch, &new_head.op_id, &attestable, &op_effects,
            )
        })
    }

    /// Open the attestation log rooted at this store. The log lives
    /// under `<root>/attestations/`; opening is idempotent and cheap
    /// (`fs::create_dir_all`). Exposed publicly so consumers — `lex
    /// blame --with-evidence`, `GET /v1/stage/<id>/attestations` —
    /// can read what the store gate emitted without round-tripping
    /// through this crate's API surface.
    /// Recompute a producer's trust score from its recent
    /// attestation history and emit a fresh `ProducerTrust`
    /// attestation (#293). Score = `passed / (passed + failed
    /// + inconclusive)` over the last `window` attestations
    /// produced by `tool_id`, expressed in thousandths
    /// (`0..=1000`).
    ///
    /// Refuses to grant trust when the tool has an active
    /// `ProducerBlock` — the block wins as a hard veto. Returns
    /// `Ok(None)` for "no attestations to score" (a brand-new
    /// producer); the caller can choose how to handle it
    /// (typically: skip the publish until evidence accrues).
    ///
    /// `granted_by` is the identity of the actor running the
    /// recompute (typically the human admin, or "lex-ci-bot"
    /// for an automated nightly).
    pub fn recompute_producer_trust(
        &self,
        tool_id: &str,
        window: usize,
        granted_by: &str,
    ) -> Result<Option<lex_vcs::AttestationId>, StoreError> {
        let log = self.attestation_log()?;
        let all = log.list_all()?;
        // Hard veto: don't grant trust to a blocked tool.
        if lex_vcs::active_producer_block(&all, tool_id).is_some() {
            return Err(StoreError::InvalidTransition(format!(
                "cannot recompute trust for `{tool_id}` — \
                 producer is currently blocked"
            )));
        }
        // Filter to attestations from this tool, newest-first by
        // timestamp, then take the window.
        let mut from_tool: Vec<&lex_vcs::Attestation> = all.iter()
            .filter(|a| a.produced_by.tool == tool_id)
            // Ignore self-referential trust attestations (we're
            // scoring evidence, not previous trust statements).
            .filter(|a| !matches!(a.kind,
                lex_vcs::AttestationKind::ProducerTrust { .. }
                | lex_vcs::AttestationKind::TrustWaived { .. }))
            .collect();
        from_tool.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
        from_tool.truncate(window);
        if from_tool.is_empty() {
            return Ok(None);
        }
        let (mut passed, mut total) = (0u64, 0u64);
        for a in &from_tool {
            total += 1;
            if matches!(a.result, lex_vcs::AttestationResult::Passed) {
                passed += 1;
            }
        }
        let score = if total == 0 {
            0
        } else {
            let raw = (passed as f64) * 1000.0 / (total as f64);
            raw.round().clamp(0.0, 1000.0) as u32
        };
        let head_op = self.list_branches()?
            .into_iter()
            .find_map(|b| self.get_branch(&b).ok().flatten().and_then(|x| x.head_op))
            .unwrap_or_else(|| "fresh".into());
        let evidence = format!("window={window}, sample={}, head_op={head_op:.16}", from_tool.len());
        let attestation = lex_vcs::Attestation::new(
            tool_id.to_string(),
            None,
            None,
            lex_vcs::AttestationKind::ProducerTrust {
                tool_id: tool_id.into(),
                score_thousandths: score,
                evidence,
                granted_by: granted_by.into(),
            },
            lex_vcs::AttestationResult::Passed,
            producer_trust_producer(),
            None,
        );
        let id = attestation.attestation_id.clone();
        log.put(&attestation)?;
        Ok(Some(id))
    }

    pub fn attestation_log(&self) -> Result<lex_vcs::AttestationLog, StoreError> {
        Ok(lex_vcs::AttestationLog::open(self.root())?)
    }

    /// Emit one `TypeCheck::Passed` attestation per stage produced by
    /// a successful gated apply. Idempotent on `attestation_id` —
    /// re-running the same gate run dedups via content addressing.
    ///
    /// Failure modes: `io::Error` from the attestation log (disk
    /// full, perms). The op has already landed by the time this
    /// runs; an error here means the op is durable but the evidence
    /// is missing. We propagate so the caller sees the partial
    /// state rather than silently swallowing — re-attesting the
    /// same op against the same op_id is idempotent (content
    /// addressing) so a retry is safe once the underlying issue is
    /// fixed.
    fn record_typecheck_passed(
        &self,
        stage_ids: &[String],
        op_id: &lex_vcs::OpId,
    ) -> Result<(), StoreError> {
        if stage_ids.is_empty() {
            return Ok(());
        }
        let log = self.attestation_log()?;
        for stage_id in stage_ids {
            let attestation = lex_vcs::Attestation::new(
                stage_id.clone(),
                Some(op_id.clone()),
                None,
                lex_vcs::AttestationKind::TypeCheck,
                lex_vcs::AttestationResult::Passed,
                typecheck_producer(),
                None,
            );
            log.put(&attestation)?;
        }
        Ok(())
    }

    /// Consult `policy.session_budgets` for the op's session
    /// (resolved via `op.intent_id → Intent.session_id`) and
    /// refuse if applying would push the session's monotonic spend
    /// over the configured cap (#292 slice 3).
    ///
    /// Ops without an `intent_id`, or whose intent has no
    /// configured cap, return Ok without any disk read.
    fn check_session_budget(
        &self,
        op: &lex_vcs::Operation,
    ) -> Result<(), StoreError> {
        let Some(intent_id) = op.intent_id.as_deref() else { return Ok(()); };
        let intent_log = lex_vcs::IntentLog::open(self.root())?;
        let Some(intent) = intent_log.get(&intent_id.to_string())? else {
            // Dangling intent — treat as "no session" and let it
            // sail through. Slice 1's ledger already documents
            // this as graceful-degradation semantics.
            return Ok(());
        };
        let policy = crate::policy::load(self.root())?.unwrap_or_default();
        let Some(cap) = policy.session_budgets.cap_for(&intent.session_id) else {
            return Ok(());
        };
        // Recompute the session's current spend + the contribution
        // from this op. Re-running the ledger walk on every gated
        // op is O(branch history); see #292 slice 1's note about
        // a future on-disk cache.
        let current = self.session_budget(&intent.session_id)?;
        let increment = crate::budget::monotonic_spend_of(&op.kind);
        let spent_after = current.spent.saturating_add(increment);
        if spent_after > cap {
            return Err(StoreError::BudgetExceeded {
                session_id: intent.session_id,
                cap,
                spent_after,
            });
        }
        Ok(())
    }

    /// Emit `RepairHint` attestations for a TypeError-rejected op
    /// (#281). One per candidate stage in the transition. The hint
    /// records the *would-be* op_id (deterministic, content-
    /// addressed even though the op record was never persisted)
    /// and the structured errors.
    ///
    /// #306 slice 3: `suggested_transform` is populated from the
    /// static (rule_tag → likely_transform) table for the *first*
    /// error in the batch. The LLM-driven `lex repair --apply`
    /// flow can still overwrite this with a higher-quality
    /// suggestion; the static value is the floor, not the ceiling.
    ///
    /// Best-effort: a write failure here is swallowed by the
    /// caller (the original `TypeError` is the load-bearing
    /// signal; missing the hint is recoverable on a retry).
    fn record_repair_hint(
        &self,
        stage_ids: &[String],
        failed_op_id: &lex_vcs::OpId,
        errors: &[lex_types::TypeError],
    ) -> Result<(), StoreError> {
        if stage_ids.is_empty() {
            return Ok(());
        }
        let errors_json = serde_json::to_value(errors)
            .map_err(StoreError::Serde)?;
        // #306 slice 3: look up the static suggested_transform for
        // the first error's rule_tag. Multiple errors per op are
        // possible — when they fire in lockstep (e.g. one bad let
        // binding propagates to several use sites), the first
        // error's rule_tag is usually the load-bearing one to fix.
        let suggested_transform = errors
            .first()
            .and_then(|e| lex_types::suggested_transform_for(e.rule_tag()));
        let log = self.attestation_log()?;
        for stage_id in stage_ids {
            let attestation = lex_vcs::Attestation::new(
                stage_id.clone(),
                None,  // the failed op was never persisted; not the
                       // attestation's op_id (which is for a
                       // *successful* op).
                None,
                lex_vcs::AttestationKind::RepairHint {
                    failed_op_id: failed_op_id.clone(),
                    errors: errors_json.clone(),
                    suggested_transform: suggested_transform.clone(),
                },
                lex_vcs::AttestationResult::Failed {
                    detail: format!("op {} rejected: {} type error(s)",
                        failed_op_id, errors.len()),
                },
                repair_hint_producer(),
                None,
            );
            log.put(&attestation)?;
        }
        Ok(())
    }

    /// Emit `Trace` attestations linking an already-committed `op`
    /// to the run that produced it (#257). One attestation per
    /// produced stage (matching the `TypeCheck` emission contract
    /// — see [`Self::apply_operation_checked`]) with
    /// `op_id: Some(op_id)` set, so `lex trace --op <op_id>`
    /// surfaces the run.
    ///
    /// Returns the number of attestations emitted (zero for ops
    /// that produce no attestable stage, e.g. `Remove` /
    /// `ImportOnly`).
    ///
    /// Idempotent: re-emitting for the same
    /// `(run_id, root_target, op_id, stage_id, producer, result)`
    /// tuple dedups via content addressing.
    ///
    /// `op_id` must already exist in the op log — an unknown op
    /// surfaces as `StoreError::UnknownOp`.
    pub fn record_op_trace(
        &self,
        run_id: &str,
        root_target: &str,
        op_id: &lex_vcs::OpId,
        result: lex_vcs::AttestationResult,
        producer: lex_vcs::ProducerDescriptor,
    ) -> Result<usize, StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let rec = log.get(op_id)?
            .ok_or_else(|| StoreError::UnknownOp(op_id.clone()))?;
        let stage_ids = attestable_stage_ids(&rec.produces);
        if stage_ids.is_empty() {
            return Ok(0);
        }
        let attlog = self.attestation_log()?;
        let mut emitted = 0;
        for stage_id in stage_ids {
            let attestation = lex_vcs::Attestation::new(
                stage_id,
                Some(op_id.clone()),
                None,
                lex_vcs::AttestationKind::Trace {
                    run_id: run_id.into(),
                    root_target: root_target.into(),
                },
                result.clone(),
                producer.clone(),
                None,
            );
            attlog.put(&attestation)?;
            emitted += 1;
        }
        Ok(emitted)
    }

    /// Walk `ops_since(branch_head, base)` and emit per-stage
    /// `Trace` attestations for each new op, linking them to the
    /// run that produced them (#257). Used by `lex run --trace`
    /// after the VM exits: snapshot `base = branch_head` before
    /// the run, then call this with the post-run head.
    ///
    /// `base = None` means "every op currently reachable from the
    /// branch head" — generally not what you want for a single
    /// run; pass the pre-run head.
    ///
    /// Returns the total number of attestations emitted across
    /// every new op. Zero is the common case (the run committed no
    /// ops).
    ///
    /// Idempotent on the per-op level via [`Self::record_op_trace`].
    pub fn record_run_committed_ops_since(
        &self,
        run_id: &str,
        root_target: &str,
        branch: &str,
        base: Option<&lex_vcs::OpId>,
        result: lex_vcs::AttestationResult,
        producer: lex_vcs::ProducerDescriptor,
    ) -> Result<usize, StoreError> {
        let head = match self.get_branch(branch)?.and_then(|b| b.head_op) {
            Some(h) => h,
            None => return Ok(0),
        };
        let log = lex_vcs::OpLog::open(self.root())?;
        let new_ops = log.ops_since(&head, base)?;
        let mut total = 0;
        for rec in new_ops {
            total += self.record_op_trace(
                run_id, root_target, &rec.op_id,
                result.clone(), producer.clone(),
            )?;
        }
        Ok(total)
    }

    /// Apply a typed `ReplaceMatchArm` transform (#280) and emit a
    /// `OperationKind::ReplaceMatchArm` op that records the
    /// semantic shape of the edit, not just the byte effect.
    ///
    /// Steps:
    ///   1. Load the source stage's canonical bytes (delta-aware).
    ///   2. Run [`lex_ast::replace_match_arm`] to produce the new
    ///      `Stage`. Pure function, no I/O.
    ///   3. Publish the new stage. Idempotent on the
    ///      content-addressed `to_stage_id`.
    ///   4. Assemble the candidate program (every active stage on
    ///      the branch, with the rewritten one swapped in) and call
    ///      [`Self::apply_operation_checked`] — re-typechecks and
    ///      runs every existing gate (TypeCheck attestation,
    ///      required_attestations, producer-block walk-back).
    ///
    /// Failure modes:
    ///   * [`StoreError::TransformError`] — transform didn't apply.
    ///     The branch is unchanged; no stage published.
    ///   * [`StoreError::TypeError`] — transform produced an
    ///     ill-typed program. The new stage is on disk (idempotent
    ///     on its content hash) but the branch is unchanged. Same
    ///     "publish without advance" semantics as #245.
    ///   * Everything else from `apply_operation_checked`.
    pub fn apply_replace_match_arm(
        &self,
        branch: &str,
        from_stage_id: &str,
        match_node: &lex_ast::NodeId,
        arm_index: usize,
        new_body: lex_ast::CExpr,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let from_stage = self.get_ast(from_stage_id)?;
        let new_stage = lex_ast::replace_match_arm(
            &from_stage, match_node, arm_index, new_body,
        ).map_err(StoreError::TransformError)?;
        let sig = lex_ast::sig_id(&from_stage)
            .ok_or(StoreError::CannotPublishImport)?;
        let to_stage_id = self.publish(&new_stage)?;
        if to_stage_id == from_stage_id {
            // No-op transform — the new body was structurally
            // identical to the old. Refuse rather than advancing
            // the branch with an empty edit.
            return Err(StoreError::InvalidTransition(format!(
                "replace_match_arm produced the same stage_id `{from_stage_id}`"
            )));
        }

        // Assemble the candidate program: every active stage on
        // the branch, with `from_stage_id` swapped for `new_stage`.
        let head = self.branch_head(branch)?;
        let mut candidate: Vec<lex_ast::Stage> = Vec::with_capacity(head.len());
        for (other_sig, other_stage_id) in &head {
            if other_sig == &sig {
                candidate.push(new_stage.clone());
            } else {
                candidate.push(self.get_ast(other_stage_id)?);
            }
        }
        // If the source sig isn't on the current branch head, the
        // transform is operating on a stage that hasn't been added
        // yet — refuse rather than risking a candidate program
        // that doesn't reflect the branch's actual state.
        if !head.contains_key(&sig) {
            return Err(StoreError::InvalidTransition(format!(
                "sig `{sig}` not on branch `{branch}`'s head"
            )));
        }

        // #247: budget delta captured for `lex op log --budget-drift`.
        let from_budget = budget_of_stage(&from_stage);
        let to_budget = budget_of_stage(&new_stage);

        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let kind = lex_vcs::OperationKind::ReplaceMatchArm {
            sig_id: sig.clone(),
            from_stage_id: from_stage_id.to_string(),
            to_stage_id: to_stage_id.clone(),
            match_node: match_node.as_str().to_string(),
            arm_index,
            from_budget,
            to_budget,
        };
        let transition = lex_vcs::StageTransition::Replace {
            sig_id: sig.clone(),
            from: from_stage_id.to_string(),
            to: to_stage_id.clone(),
        };
        let op = lex_vcs::Operation::new(
            kind,
            head_now.into_iter().collect::<Vec<_>>(),
        );
        self.apply_operation_checked(branch, op, transition, &candidate)
    }

    /// Apply a typed `RenameLocal` transform (#280) — rename a
    /// `let`-bound local within a fn body and emit a matching
    /// `OperationKind::RenameLocal`. Same end-to-end shape as
    /// [`Self::apply_replace_match_arm`]; see that method for the
    /// failure-mode taxonomy.
    pub fn apply_rename_local(
        &self,
        branch: &str,
        from_stage_id: &str,
        let_node: &lex_ast::NodeId,
        new_name: &str,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let from_stage = self.get_ast(from_stage_id)?;
        // Read the old name before running the transform, so the
        // op log records the rename target rather than just the
        // new value.
        let old_name = read_let_name(&from_stage, let_node)
            .map_err(StoreError::TransformError)?;
        let new_stage = lex_ast::rename_local(&from_stage, let_node, new_name)
            .map_err(StoreError::TransformError)?;
        let sig = lex_ast::sig_id(&from_stage)
            .ok_or(StoreError::CannotPublishImport)?;
        let to_stage_id = self.publish(&new_stage)?;
        if to_stage_id == from_stage_id {
            return Err(StoreError::InvalidTransition(format!(
                "rename_local produced the same stage_id `{from_stage_id}`"
            )));
        }
        let head = self.branch_head(branch)?;
        let mut candidate: Vec<lex_ast::Stage> = Vec::with_capacity(head.len());
        for (other_sig, other_stage_id) in &head {
            if other_sig == &sig {
                candidate.push(new_stage.clone());
            } else {
                candidate.push(self.get_ast(other_stage_id)?);
            }
        }
        if !head.contains_key(&sig) {
            return Err(StoreError::InvalidTransition(format!(
                "sig `{sig}` not on branch `{branch}`'s head"
            )));
        }
        let from_budget = budget_of_stage(&from_stage);
        let to_budget = budget_of_stage(&new_stage);
        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let kind = lex_vcs::OperationKind::RenameLocal {
            sig_id: sig.clone(),
            from_stage_id: from_stage_id.to_string(),
            to_stage_id: to_stage_id.clone(),
            let_node: let_node.as_str().to_string(),
            old_name,
            new_name: new_name.to_string(),
            from_budget,
            to_budget,
        };
        let transition = lex_vcs::StageTransition::Replace {
            sig_id: sig.clone(),
            from: from_stage_id.to_string(),
            to: to_stage_id.clone(),
        };
        let op = lex_vcs::Operation::new(
            kind,
            head_now.into_iter().collect::<Vec<_>>(),
        );
        self.apply_operation_checked(branch, op, transition, &candidate)
    }

    /// Apply a typed `InlineLet` transform (#280) — eliminate a
    /// `let x := v; body` by substituting `v` for every unshadowed
    /// `x` in `body`, then replacing the `Let` node with the
    /// substituted body. Same end-to-end shape as
    /// [`Self::apply_replace_match_arm`].
    pub fn apply_inline_let(
        &self,
        branch: &str,
        from_stage_id: &str,
        let_node: &lex_ast::NodeId,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let from_stage = self.get_ast(from_stage_id)?;
        let binding_name = read_let_name(&from_stage, let_node)
            .map_err(StoreError::TransformError)?;
        let new_stage = lex_ast::inline_let(&from_stage, let_node)
            .map_err(StoreError::TransformError)?;
        let sig = lex_ast::sig_id(&from_stage)
            .ok_or(StoreError::CannotPublishImport)?;
        let to_stage_id = self.publish(&new_stage)?;
        if to_stage_id == from_stage_id {
            return Err(StoreError::InvalidTransition(format!(
                "inline_let produced the same stage_id `{from_stage_id}`"
            )));
        }
        let head = self.branch_head(branch)?;
        let mut candidate: Vec<lex_ast::Stage> = Vec::with_capacity(head.len());
        for (other_sig, other_stage_id) in &head {
            if other_sig == &sig {
                candidate.push(new_stage.clone());
            } else {
                candidate.push(self.get_ast(other_stage_id)?);
            }
        }
        if !head.contains_key(&sig) {
            return Err(StoreError::InvalidTransition(format!(
                "sig `{sig}` not on branch `{branch}`'s head"
            )));
        }
        let from_budget = budget_of_stage(&from_stage);
        let to_budget = budget_of_stage(&new_stage);
        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let kind = lex_vcs::OperationKind::InlineLet {
            sig_id: sig.clone(),
            from_stage_id: from_stage_id.to_string(),
            to_stage_id: to_stage_id.clone(),
            let_node: let_node.as_str().to_string(),
            binding_name,
            from_budget,
            to_budget,
        };
        let transition = lex_vcs::StageTransition::Replace {
            sig_id: sig.clone(),
            from: from_stage_id.to_string(),
            to: to_stage_id.clone(),
        };
        let op = lex_vcs::Operation::new(
            kind,
            head_now.into_iter().collect::<Vec<_>>(),
        );
        self.apply_operation_checked(branch, op, transition, &candidate)
    }

    /// Apply a typed `ExtractFunction` transform (#280 slice 4) —
    /// extract a sub-expression of `from_stage_id`'s body into a
    /// new top-level fn defined by `spec`, and emit two ops tied
    /// together by a shared synthetic Intent so `lex op log
    /// --intent <id>` groups them.
    ///
    /// The two ops:
    ///   1. `AddFunction { sig_id: <new_fn_sig>, stage_id: <new_fn_stage> }`
    ///   2. `ModifyBody { sig_id: <source_sig>, from_stage_id, to_stage_id: <modified> }`
    ///
    /// The shared Intent's prompt is structured (`extract_function:
    /// <new_fn_name>` plus the source identity) so downstream
    /// tooling can recover the typed-transform shape from the
    /// op-log + intent-log join.
    ///
    /// Returns `(add_fn_op_id, modify_body_op_id)`.
    pub fn apply_extract_function(
        &self,
        branch: &str,
        from_stage_id: &str,
        expr_node: &lex_ast::NodeId,
        spec: lex_ast::ExtractFnSpec,
    ) -> Result<(lex_vcs::OpId, lex_vcs::OpId), StoreError> {
        let from_stage = self.get_ast(from_stage_id)?;
        let new_fn_name = spec.name.clone();
        let (modified_stage, new_fn_stage) =
            lex_ast::extract_function(&from_stage, expr_node, spec)
                .map_err(StoreError::TransformError)?;

        let source_sig = lex_ast::sig_id(&from_stage)
            .ok_or(StoreError::CannotPublishImport)?;
        let new_fn_sig = lex_ast::sig_id(&new_fn_stage)
            .ok_or(StoreError::CannotPublishImport)?;
        if source_sig == new_fn_sig {
            return Err(StoreError::InvalidTransition(format!(
                "extract_function produced a sig matching the source `{source_sig}`"
            )));
        }
        let new_fn_stage_id = self.publish(&new_fn_stage)?;
        let modified_stage_id = self.publish(&modified_stage)?;
        if modified_stage_id == from_stage_id {
            return Err(StoreError::InvalidTransition(format!(
                "extract_function produced the same stage_id `{from_stage_id}` for the source"
            )));
        }

        let head = self.branch_head(branch)?;
        if !head.contains_key(&source_sig) {
            return Err(StoreError::InvalidTransition(format!(
                "sig `{source_sig}` not on branch `{branch}`'s head"
            )));
        }

        // Synthesize an Intent linking the two ops. The session_id
        // / model fields here are not load-bearing — they exist to
        // make the IntentId content-addressed; downstream tooling
        // reads `prompt` to reconstruct the typed-transform shape.
        let intent = lex_vcs::Intent::new(
            format!(
                "[lex.transform.extract_function]\nnew_fn={new_fn_name}\nsource_sig={source_sig}\nfrom_stage={from_stage_id}\nexpr_node={node}",
                node = expr_node.as_str(),
            ),
            "lex-store::apply_extract_function",
            lex_vcs::ModelDescriptor {
                provider: "lex-store".into(),
                name: env!("CARGO_PKG_VERSION").into(),
                version: None,
            },
            None,
        );
        let intent_id = intent.intent_id.clone();
        lex_vcs::IntentLog::open(self.root())?.put(&intent)?;

        // Step 1 — emit the AddFunction op for the new fn. Build
        // the candidate program by appending the new fn to every
        // stage on the current branch head.
        let new_fn_effects: std::collections::BTreeSet<String> = match &new_fn_stage {
            lex_ast::Stage::FnDecl(fd) => fd.effects.iter()
                .map(|e| e.name.clone()).collect(),
            _ => Default::default(),
        };
        let new_fn_budget = budget_of_stage(&new_fn_stage);
        let mut candidate_with_new_fn: Vec<lex_ast::Stage> =
            Vec::with_capacity(head.len() + 1);
        for stage_id in head.values() {
            candidate_with_new_fn.push(self.get_ast(stage_id)?);
        }
        candidate_with_new_fn.push(new_fn_stage.clone());
        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let add_op = lex_vcs::Operation::new(
            lex_vcs::OperationKind::AddFunction {
                sig_id: new_fn_sig.clone(),
                stage_id: new_fn_stage_id.clone(),
                effects: new_fn_effects,
                budget_cost: new_fn_budget,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        ).with_intent(intent_id.clone());
        let add_transition = lex_vcs::StageTransition::Create {
            sig_id: new_fn_sig.clone(),
            stage_id: new_fn_stage_id.clone(),
        };
        let add_op_id = self.apply_operation_checked(
            branch, add_op, add_transition, &candidate_with_new_fn,
        )?;

        // Step 2 — emit the ModifyBody op for the source. Build
        // the candidate program by replacing the source's stage
        // with `modified_stage` and keeping the new fn alongside.
        let from_budget = budget_of_stage(&from_stage);
        let to_budget = budget_of_stage(&modified_stage);
        let mut candidate_with_modified: Vec<lex_ast::Stage> =
            Vec::with_capacity(head.len() + 1);
        for (other_sig, other_stage_id) in &head {
            if other_sig == &source_sig {
                candidate_with_modified.push(modified_stage.clone());
            } else {
                candidate_with_modified.push(self.get_ast(other_stage_id)?);
            }
        }
        candidate_with_modified.push(new_fn_stage.clone());
        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let modify_op = lex_vcs::Operation::new(
            lex_vcs::OperationKind::ModifyBody {
                sig_id: source_sig.clone(),
                from_stage_id: from_stage_id.to_string(),
                to_stage_id: modified_stage_id.clone(),
                from_budget,
                to_budget,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        ).with_intent(intent_id);
        let modify_transition = lex_vcs::StageTransition::Replace {
            sig_id: source_sig,
            from: from_stage_id.to_string(),
            to: modified_stage_id,
        };
        let modify_op_id = self.apply_operation_checked(
            branch, modify_op, modify_transition, &candidate_with_modified,
        )?;

        Ok((add_op_id, modify_op_id))
    }

    /// Propose a stage for `sig_id` without advancing the branch
    /// head (#294). Multiple agents can call this concurrently
    /// for the same sig — every call lands a fresh `Candidate`
    /// op chained off the current head_op. The branch head stays
    /// where it was; a later [`Self::promote_candidate`] picks
    /// the winner.
    ///
    /// The caller is responsible for typechecking `new_stage`
    /// against whatever program context they consider valid —
    /// `propose_candidate` doesn't run the gate. Type errors
    /// surface at promotion time, where the candidate is
    /// composed back into a candidate program via the standard
    /// `apply_operation_checked` path.
    ///
    /// The stage is published (idempotent on content hash). The
    /// `intent_id` is required so downstream consumers can
    /// distinguish proposals by author.
    pub fn propose_candidate(
        &self,
        branch: &str,
        new_stage: &lex_ast::Stage,
        intent_id: &lex_vcs::IntentId,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let sig = lex_ast::sig_id(new_stage)
            .ok_or(StoreError::CannotPublishImport)?;
        let stage_id = self.publish(new_stage)?;
        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let op = lex_vcs::Operation::new(
            lex_vcs::OperationKind::Candidate {
                sig_id: sig,
                stage_id,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        ).with_intent(intent_id.clone());
        let transition = lex_vcs::StageTransition::ImportOnly;
        self.apply_operation(branch, op, transition)
    }

    /// List every live `Candidate` op for `sig_id` — i.e. those
    /// not yet referenced by any `Promote` op (either as the
    /// winner or in the `supersedes` set). Used by `lex stage
    /// candidates`. Results are sorted by op_id for
    /// reproducibility.
    pub fn list_candidates(&self, sig_id: &str) -> Result<Vec<CandidateInfo>, StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let all = log.list_all()?;
        // Collect the set of candidate op_ids referenced by any
        // Promote for this sig. Those candidates are no longer
        // live.
        let mut referenced: std::collections::BTreeSet<lex_vcs::OpId> = Default::default();
        for rec in &all {
            if let lex_vcs::OperationKind::Promote { sig_id: s, winner_candidate, supersedes, .. } = &rec.op.kind {
                if s != sig_id { continue; }
                referenced.insert(winner_candidate.clone());
                for sup in supersedes { referenced.insert(sup.clone()); }
            }
        }
        let mut out: Vec<CandidateInfo> = Vec::new();
        for rec in all {
            let lex_vcs::OperationKind::Candidate { sig_id: s, stage_id } = &rec.op.kind
                else { continue };
            if s != sig_id { continue; }
            if referenced.contains(&rec.op_id) { continue; }
            out.push(CandidateInfo {
                op_id: rec.op_id.clone(),
                stage_id: stage_id.clone(),
                intent_id: rec.op.intent_id.clone(),
            });
        }
        out.sort_by(|a, b| a.op_id.cmp(&b.op_id));
        Ok(out)
    }

    /// Promote a previously-landed `Candidate` op as the new
    /// branch head for its sig (#294). Emits a `Promote` op
    /// listing every other live `Candidate` for the same sig
    /// in its `supersedes` field. After this lands,
    /// [`Self::list_candidates`] returns an empty set for the
    /// sig.
    ///
    /// Re-typechecks the candidate program (winner stage + the
    /// rest of the branch) through `apply_operation_checked`, so
    /// a candidate that doesn't compose with the current branch
    /// state surfaces as `StoreError::TypeError`.
    pub fn promote_candidate(
        &self,
        branch: &str,
        candidate_op_id: &lex_vcs::OpId,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let candidate_rec = log.get(candidate_op_id)?
            .ok_or_else(|| StoreError::UnknownOp(candidate_op_id.clone()))?;
        let (sig, winner_stage_id) = match &candidate_rec.op.kind {
            lex_vcs::OperationKind::Candidate { sig_id, stage_id } =>
                (sig_id.clone(), stage_id.clone()),
            other => return Err(StoreError::InvalidTransition(format!(
                "op `{candidate_op_id}` is a `{:?}`, not a Candidate", other
            ))),
        };

        // Gather every OTHER live candidate for this sig — the
        // ones this Promote will supersede.
        let live = self.list_candidates(&sig)?;
        let mut supersedes: Vec<lex_vcs::OpId> = live.iter()
            .filter(|c| &c.op_id != candidate_op_id)
            .map(|c| c.op_id.clone())
            .collect();
        supersedes.sort();

        // Assemble candidate program: winner stage in place of
        // the sig's current head (if any), plus every other sig
        // unchanged.
        let head = self.branch_head(branch)?;
        let winner_stage = self.get_ast(&winner_stage_id)?;
        let mut candidate_program: Vec<lex_ast::Stage> = Vec::with_capacity(head.len() + 1);
        let mut found = false;
        for (other_sig, other_stage_id) in &head {
            if other_sig == &sig {
                candidate_program.push(winner_stage.clone());
                found = true;
            } else {
                candidate_program.push(self.get_ast(other_stage_id)?);
            }
        }
        if !found {
            // Sig doesn't have a head yet — append the winner
            // stage to make it a Create.
            candidate_program.push(winner_stage.clone());
        }
        let from_stage_id = head.get(&sig).cloned();
        // Budget delta from old head to winner — same shape as
        // ModifyBody.
        let from_budget = from_stage_id.as_deref()
            .and_then(|s| self.get_ast(s).ok())
            .and_then(|s| budget_of_stage(&s));
        let to_budget = budget_of_stage(&winner_stage);

        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let op = lex_vcs::Operation::new(
            lex_vcs::OperationKind::Promote {
                sig_id: sig.clone(),
                winner_candidate: candidate_op_id.clone(),
                winner_stage_id: winner_stage_id.clone(),
                supersedes,
                from_stage_id: from_stage_id.clone(),
                from_budget,
                to_budget,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        );
        let transition = match &from_stage_id {
            Some(from) => lex_vcs::StageTransition::Replace {
                sig_id: sig,
                from: from.clone(),
                to: winner_stage_id,
            },
            None => lex_vcs::StageTransition::Create {
                sig_id: sig,
                stage_id: winner_stage_id,
            },
        };
        self.apply_operation_checked(branch, op, transition, &candidate_program)
    }

    /// `set_branch_head_op` for the durability story on the branch
    /// file itself.
    pub fn apply_operation(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let attestable = attestable_stage_ids(&transition);
        let op_effects = op_declared_effects(&op.kind);
        self.cas_retry_advance(branch, op, transition, |new_head| {
            self.run_required_attestations_gate(
                branch, &new_head.op_id, &attestable, &op_effects,
            )
        })
    }

    /// CAS retry loop for #262. Single-parent ops are rebuilt on
    /// each iteration with the current branch head as parent;
    /// the per-iteration callback runs the gate (and TypeCheck
    /// emission, for the checked path) between persist and CAS.
    /// Merge ops (with 2 parents already set) skip the rebuild —
    /// their parents are caller-supplied and meaningful — and get
    /// a single attempt; on CAS failure they surface `Contention`.
    fn cas_retry_advance<F>(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
        mut between_persist_and_cas: F,
    ) -> Result<lex_vcs::OpId, StoreError>
    where
        F: FnMut(&lex_vcs::NewHead) -> Result<(), StoreError>,
    {
        // 32 retries handles up to ~32 concurrent writers racing on
        // the same branch tip. Beyond that, surfacing `Contention`
        // is the right signal — clients should back off or batch.
        const MAX_ATTEMPTS: u32 = 32;
        // Single-parent ops can be rebuilt on retry; merge ops
        // can't (their two parents are meaningful, supplied by the
        // merge engine). For merges, single attempt: if CAS
        // fails, surface Contention.
        let is_rebuildable = op.parents.len() <= 1;
        let kind = op.kind.clone();
        let intent_id = op.intent_id.clone();

        let mut last_io_err: Option<StoreError> = None;
        let mut current_op = op;
        let current_transition = transition;
        // Only rebuild on retries — attempt 1 honors the caller's
        // exact op so a user-supplied bogus parent (parents =
        // ["someone-else"]) surfaces as `StaleParent` instead of
        // being silently corrected.
        //
        // Exception (#262 follow-up): an op with `parents = []`
        // means "I don't care; chain off whatever the current
        // head is." Under concurrent apply, attempt 1 can read
        // `head_op = Some(opA)` after a sibling writer landed,
        // and the persist's parent check fails StaleParent
        // unprompted. Rebuild attempt 1 for the empty-parents
        // case so the legitimate-race path retries cleanly.
        let mut rebuilt_already = false;
        for attempt in 1..=MAX_ATTEMPTS {
            // Read the current head BEFORE we persist — this is
            // the value we'll compare against in the CAS.
            let parent = self
                .get_branch(branch)?
                .and_then(|b| b.head_op);

            // Rebuild the op against the current head, but only
            // on retries (not the caller's first attempt) and
            // only for single-parent operations. Multi-parent
            // (merge) ops are passed through unchanged.
            //
            // Empty-parents ops also rebuild on attempt 1 (see
            // the exception note above) so concurrent apply
            // doesn't false-positive on StaleParent.
            let should_rebuild = is_rebuildable
                && (rebuilt_already
                    || (current_op.parents.is_empty() && parent.is_some()));
            if should_rebuild {
                current_op = lex_vcs::Operation {
                    kind: kind.clone(),
                    parents: parent.iter().cloned().collect(),
                    intent_id: intent_id.clone(),
                };
            }

            // Persist (idempotent). On `StaleParent` from a retry
            // attempt (where we already rebuilt), the head changed
            // between our `get_branch` and this `lex_vcs::apply`
            // — race; rebuild and continue. On `StaleParent` from
            // attempt 1 (caller's input), propagate.
            let new_head = match self.persist_op_only_with_parent(
                branch,
                parent.as_ref(),
                current_op.clone(),
                current_transition.clone(),
            ) {
                Ok(nh) => nh,
                Err(StoreError::Apply(lex_vcs::ApplyError::StaleParent { .. }))
                    if is_rebuildable && rebuilt_already =>
                {
                    rebuilt_already = true;
                    continue;
                }
                Err(e) => return Err(e),
            };

            // Run the caller's between-persist-and-cas hook
            // (TypeCheck emission + gate). If this fails, the op
            // record is durable but orphaned — same semantics as
            // pre-#262.
            between_persist_and_cas(&new_head)?;

            // CAS the branch head. On success: done. On mismatch:
            // someone advanced in parallel; retry.
            match self.set_branch_head_op_cas(branch, parent, new_head.op_id.clone()) {
                Ok(()) => return Ok(new_head.op_id),
                Err(crate::branches::CasFailed::Mismatch { .. }) if is_rebuildable => {
                    // Try again with the new head as parent.
                    rebuilt_already = true;
                    continue;
                }
                Err(crate::branches::CasFailed::Mismatch { .. }) => {
                    // Merge op: surface immediately — we can't
                    // rebuild without rerunning the merge engine.
                    let _ = attempt;
                    return Err(StoreError::Contention {
                        branch: branch.into(),
                        attempts: 1,
                    });
                }
                Err(crate::branches::CasFailed::UnknownBranch(b)) => {
                    return Err(StoreError::UnknownBranch(b));
                }
                Err(crate::branches::CasFailed::Io(e)) => {
                    last_io_err = Some(StoreError::Io(std::io::Error::other(e)));
                    continue;
                }
            }
        }
        // Retries exhausted. Prefer surfacing the most recent IO
        // error if we hit one; otherwise it's pure CAS contention.
        match last_io_err {
            Some(e) => Err(e),
            None => Err(StoreError::Contention {
                branch: branch.into(),
                attempts: MAX_ATTEMPTS,
            }),
        }
    }

    /// Persist an op against an explicitly-supplied parent. Used
    /// by the CAS retry loop in `cas_retry_advance` so the
    /// `lex_vcs::apply` parent check matches what we read at the
    /// top of the loop iteration (avoids a TOCTOU race against
    /// `persist_op_only`'s second read).
    fn persist_op_only_with_parent(
        &self,
        branch: &str,
        parent: Option<&lex_vcs::OpId>,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
    ) -> Result<lex_vcs::NewHead, StoreError> {
        if branch != DEFAULT_BRANCH && self.get_branch(branch)?.is_none() {
            return Err(StoreError::UnknownBranch(branch.into()));
        }
        let log = lex_vcs::OpLog::open(self.root())?;
        lex_vcs::apply(&log, parent, op, transition).map_err(|e| match e {
            lex_vcs::ApplyError::Persist(io) => StoreError::Io(io),
            other => StoreError::Apply(other),
        })
    }


    /// Run the `required_attestations` gate (#245) and the
    /// retroactive producer-block gate (#248) over a single op
    /// against the store's `policy.json` and attestation log.
    ///
    /// Failure modes (in order):
    ///
    /// 1. Producer-block first: if any attestation on the op's
    ///    stage is from a quarantined tool, refuse with
    ///    `ProducerBlocked` (#248). Surfaces *before* the
    ///    required-attestations gate so a clearly-malicious record
    ///    isn't masked by a missing-Spec error.
    /// 2. Required-attestations next: if any required attestation
    ///    kind is missing, refuse with `BranchAdvanceBlocked`
    ///    (#245).
    ///
    /// Loads the policy / attestation log lazily; with no policy
    /// file and no `ProducerBlock` attestations the gate is a no-op
    /// (default-permissive — matches pre-#245 stores).
    fn run_required_attestations_gate(
        &self,
        branch: &str,
        op_id: &lex_vcs::OpId,
        stage_ids: &[String],
        op_effects: &std::collections::BTreeSet<String>,
    ) -> Result<(), StoreError> {
        // Build the candidate slice for the new op. Ops with no
        // attestable stage (imports, empty merges) get a single
        // `None`-stage tuple; both gates skip those.
        let new_op_candidate: Vec<(lex_vcs::OpId, Option<String>, std::collections::BTreeSet<String>)> =
            if stage_ids.is_empty() {
                vec![(op_id.clone(), None, op_effects.clone())]
            } else {
                stage_ids
                    .iter()
                    .map(|sid| (op_id.clone(), Some(sid.clone()), op_effects.clone()))
                    .collect()
            };
        let attest_log = self.attestation_log()?;

        // #248 + #256: producer-block gate, walk-back style.
        //
        // The naive #248 gate only checked the new op's stage. That
        // missed contamination on ancestors — once `lex attest
        // retro-block` lands, every previously-gated op stays in
        // the chain even though its attestations are now from a
        // quarantined producer.
        //
        // #256 fixes this by walking the chain from `head_op` back
        // to `last_gate_checkpoint` (or genesis when the checkpoint
        // is invalidated), collecting each ancestor's attestable
        // stages, and running `check_producer_block` on the
        // combined set. After a successful advance,
        // `set_branch_head_op` moves the checkpoint to the new
        // head (steady-state O(new ops) per advance).
        let walk_back_candidate = self.collect_ancestor_candidates(branch)?;
        let mut producer_block_candidate = walk_back_candidate;
        producer_block_candidate.extend(new_op_candidate.iter().cloned());
        crate::policy::check_producer_block(&attest_log, &producer_block_candidate)
            .map_err(StoreError::ProducerBlocked)?;

        // #245: required-attestations gate. Forward-going only —
        // only the new op is checked. Walking back makes no sense
        // here: the policy is "this advance must carry these
        // attestations," not "every prior op must have."
        let policy = match crate::policy::load(self.root())? {
            Some(p) if !p.required_attestations.is_empty() => p,
            _ => return Ok(()),
        };
        let waivers = crate::policy::check_required_attestations(
            &attest_log, &new_op_candidate, &policy,
        ).map_err(StoreError::BranchAdvanceBlocked)?;
        // #293: emit one `TrustWaived` attestation per waiver so
        // the audit trail records every skip. Idempotent on
        // attestation_id (content-addressed dedup) — re-running
        // the gate with the same state writes the same files.
        for w in waivers {
            let att = lex_vcs::Attestation::new(
                w.stage_id,
                Some(op_id.clone()),
                None,
                lex_vcs::AttestationKind::TrustWaived {
                    producer: w.producer,
                    score_thousandths: w.score_thousandths,
                    threshold_thousandths: w.threshold_thousandths,
                    kind_tag: w.kind_tag,
                },
                lex_vcs::AttestationResult::Passed,
                trust_waived_producer(),
                None,
            );
            attest_log.put(&att)?;
        }
        Ok(())
    }

    /// Walk the branch from `head_op` back to `last_gate_checkpoint`
    /// (exclusive) and return the `(op_id, stage_id, op_effects)`
    /// tuples for every attestable stage touched by an ancestor
    /// (#256). Empty when the branch is fresh, when the checkpoint
    /// equals the head, or when the head is None.
    fn collect_ancestor_candidates(
        &self,
        branch: &str,
    ) -> Result<Vec<GateCandidate>, StoreError> {
        let b = match self.get_branch(branch)? {
            Some(b) => b,
            None => return Ok(Vec::new()),
        };
        let Some(head) = b.head_op else { return Ok(Vec::new()); };
        if Some(&head) == b.last_gate_checkpoint.as_ref() {
            // Steady-state common case: previous advance left the
            // checkpoint at head. Nothing to re-walk.
            return Ok(Vec::new());
        }

        let log = lex_vcs::OpLog::open(self.root())?;
        let walk = log.walk_back(&head, None)?;
        let stop_at = b.last_gate_checkpoint.clone();
        let mut out = Vec::new();
        for rec in walk {
            if Some(&rec.op_id) == stop_at.as_ref() {
                break;
            }
            let stages = attestable_stage_ids(&rec.produces);
            let effects = op_declared_effects(&rec.op.kind);
            if stages.is_empty() {
                out.push((rec.op_id.clone(), None, effects));
            } else {
                for sid in stages {
                    out.push((rec.op_id.clone(), Some(sid), effects.clone()));
                }
            }
        }
        Ok(out)
    }
}

fn stage_name(stage: &Stage) -> &str {
    match stage {
        Stage::FnDecl(fd) => &fd.name,
        Stage::TypeDecl(td) => &td.name,
        Stage::Import(i) => &i.alias,
    }
}

fn stage_for_kind<'a>(
    kind: &lex_vcs::OperationKind,
    stages: &'a [lex_ast::Stage],
) -> Option<&'a lex_ast::Stage> {
    use lex_vcs::OperationKind::*;
    let target_sig = match kind {
        AddFunction { sig_id, .. } | ModifyBody { sig_id, .. }
        | ChangeEffectSig { sig_id, .. } | AddType { sig_id, .. }
        | ModifyType { sig_id, .. } => Some(sig_id.clone()),
        RenameSymbol { to, .. } => Some(to.clone()),
        _ => None,
    };
    let target_sig = target_sig?;
    stages.iter().find(|s| sig_id(s).as_deref() == Some(target_sig.as_str()))
}

fn transition_for_kind(kind: &lex_vcs::OperationKind) -> lex_vcs::StageTransition {
    use lex_vcs::OperationKind::*;
    use lex_vcs::StageTransition;
    match kind {
        AddFunction { sig_id, stage_id, .. }
        | AddType { sig_id, stage_id } => StageTransition::Create {
            sig_id: sig_id.clone(), stage_id: stage_id.clone(),
        },
        RemoveFunction { sig_id, last_stage_id }
        | RemoveType { sig_id, last_stage_id } => StageTransition::Remove {
            sig_id: sig_id.clone(), last: last_stage_id.clone(),
        },
        ModifyBody { sig_id, from_stage_id, to_stage_id, .. }
        | ChangeEffectSig { sig_id, from_stage_id, to_stage_id, .. }
        | ModifyType { sig_id, from_stage_id, to_stage_id }
        | ReplaceMatchArm { sig_id, from_stage_id, to_stage_id, .. }
        | RenameLocal { sig_id, from_stage_id, to_stage_id, .. }
        | InlineLet { sig_id, from_stage_id, to_stage_id, .. } => StageTransition::Replace {
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
        // #294: a Candidate proposes a stage without advancing
        // the branch. ImportOnly keeps the branch head untouched
        // — the stage IS published on disk (Store::propose_candidate
        // calls publish before apply), but no head delta lands.
        Candidate { .. } => StageTransition::ImportOnly,
        // A Promote advances the head exactly like ModifyBody
        // (or Create when the sig had no head). The winner
        // stage is the new branch state for that sig.
        Promote { sig_id, winner_stage_id, from_stage_id, .. } => match from_stage_id {
            Some(from) => StageTransition::Replace {
                sig_id: sig_id.clone(),
                from: from.clone(),
                to: winner_stage_id.clone(),
            },
            None => StageTransition::Create {
                sig_id: sig_id.clone(),
                stage_id: winner_stage_id.clone(),
            },
        },
    }
}

/// Producer identity for TypeCheck attestations emitted by the
/// store-write gate. Pinned to this crate's name + version so an
/// attestation produced by a different `lex-store` revision is
/// distinguishable (content-hashed `produced_by`).
fn typecheck_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-store".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// Producer identity for `RepairHint` attestations emitted by
/// `apply_operation_checked` on TypeError (#281). Distinct tool
/// name from `typecheck_producer` so consumers can filter the
/// activity feed for repair hints without scanning kinds.
fn repair_hint_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-store::repair_hint".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// Producer identity for `TrustWaived` attestations emitted by
/// the `required_attestations` gate on a trust-driven waiver
/// (#293). Distinct from `typecheck_producer` and `repair_hint`
/// so the audit trail clearly shows "the gate let this advance
/// through because trust > threshold."
fn trust_waived_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-store::trust_waived".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// Producer identity for `ProducerTrust` attestations emitted by
/// [`Store::recompute_producer_trust`]. The score-derivation
/// recompute is its own machine-emittable kind, distinct from
/// the gate-side `TrustWaived` emit (#293).
fn producer_trust_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-store::producer_trust".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// The set of stage_ids a transition introduces. These are the
/// stages a successful TypeCheck pass attests *about* — the new
/// head produced by Create/Replace, the renamed body, or the per-
/// sig resolution of a Merge. Removes and ImportOnly produce no
/// attestable stage; the program typechecks but no specific stage
/// is the subject of the claim.
/// One row of input to the producer-block / required-attestations
/// gates: `(op_id, stage_id, op_effects)`. The `stage_id` is
/// `None` for ops that don't touch a stage (imports, empty
/// merges) — the gate skips those.
type GateCandidate = (lex_vcs::OpId, Option<String>, std::collections::BTreeSet<String>);

/// Effect set declared *by the operation itself* (#245). Used by
/// the `required_attestations` gate's `EffectsIntersect` clause.
///
/// Only `AddFunction` and `ChangeEffectSig` carry an effect set in
/// their op payload; for everything else this returns the empty
/// set, which means `EffectsIntersect` rules don't fire on those
/// ops. `Always` rules continue to fire regardless. A future
/// improvement is to extract effects from the candidate `Stage`
/// for `ModifyBody` ops, but the typed-effects-on-ops path (#247)
/// is the cleaner solution and lands separately.
fn op_declared_effects(kind: &lex_vcs::OperationKind) -> std::collections::BTreeSet<String> {
    use lex_vcs::OperationKind::*;
    match kind {
        AddFunction { effects, .. } => effects.clone(),
        ChangeEffectSig { to_effects, .. } => to_effects.clone(),
        _ => std::collections::BTreeSet::new(),
    }
}

fn attestable_stage_ids(transition: &lex_vcs::StageTransition) -> Vec<String> {
    use lex_vcs::StageTransition::*;
    match transition {
        Create { stage_id, .. } => vec![stage_id.clone()],
        Replace { to, .. } => vec![to.clone()],
        Rename { body_stage_id, .. } => vec![body_stage_id.clone()],
        Merge { entries } => entries
            .values()
            .filter_map(|opt| opt.clone())
            .collect(),
        Remove { .. } | ImportOnly => Vec::new(),
    }
}

fn write_canonical_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let v = serde_json::to_value(value)?;
    let s = lex_ast::canon_json::to_canonical_string(&v);
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    fs::write(path, s)?;
    Ok(())
}

/// Read the `name` of the `Let` expression at `let_node` inside
/// `stage`'s body. Used by [`Store::apply_rename_local`] to record
/// the rename source. Returns the same `TransformError` shapes as
/// the transformer itself so callers see a consistent error
/// vocabulary.
fn read_let_name(
    stage: &Stage,
    let_node: &lex_ast::NodeId,
) -> Result<String, lex_ast::TransformError> {
    // The transformer is itself a pure function; ask it to perform
    // a rename to a sentinel value and read the resulting let's
    // original name from the output. Cheaper than duplicating the
    // node-walk here, and stays correct as the transform evolves.
    //
    // We use a sentinel that's invalid as a Lex identifier so even
    // if the rename somehow lands, downstream parsing would
    // surface it loudly. (The transform path discards the renamed
    // value — we only need the *original* name.)
    let probed = lex_ast::rename_local(stage, let_node, "__lex_rename_probe__")?;
    let Stage::FnDecl(fd) = probed else {
        return Err(lex_ast::TransformError::NonFnTarget { stage_kind: "non-FnDecl" });
    };
    // Walk back to the probed let to read its old name from the
    // *original* stage — the probed stage's let has already been
    // renamed.
    let Stage::FnDecl(orig_fd) = stage else {
        return Err(lex_ast::TransformError::NonFnTarget { stage_kind: "non-FnDecl" });
    };
    // Path-based lookup matches the transformer's navigation.
    let path = parse_let_node_path(let_node.as_str())?;
    if path.is_empty() {
        return Err(lex_ast::TransformError::NotALet {
            at: let_node.as_str().into(),
            found_kind: "stage_root",
        });
    }
    if path[0] != orig_fd.params.len() + 1 {
        return Err(lex_ast::TransformError::UnknownNode {
            at: let_node.as_str().into(),
        });
    }
    let inner = &path[1..];
    let target = navigate_to_let(&orig_fd.body, inner, let_node.as_str())?;
    let _ = fd; // probed stage discarded
    Ok(target.to_string())
}

fn parse_let_node_path(id: &str) -> Result<Vec<usize>, lex_ast::TransformError> {
    let s = id.strip_prefix("n_")
        .ok_or_else(|| lex_ast::TransformError::BadNodeId(id.into()))?;
    let mut parts = s.split('.');
    let head = parts.next()
        .ok_or_else(|| lex_ast::TransformError::BadNodeId(id.into()))?;
    if head != "0" { return Err(lex_ast::TransformError::BadNodeId(id.into())); }
    let mut out = Vec::new();
    for p in parts {
        out.push(p.parse::<usize>()
            .map_err(|_| lex_ast::TransformError::BadNodeId(id.into()))?);
    }
    Ok(out)
}

fn navigate_to_let<'a>(
    root: &'a lex_ast::CExpr,
    path: &[usize],
    at: &str,
) -> Result<&'a str, lex_ast::TransformError> {
    use lex_ast::CExpr::*;
    let mut current = root;
    for &idx in path {
        current = match current {
            Call { callee, args } => {
                if idx == 0 { callee } else { args.get(idx - 1)
                    .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })? }
            }
            Let { value, body, .. } => match idx {
                0 => value, 1 => body,
                _ => return Err(lex_ast::TransformError::UnknownNode { at: at.into() }),
            },
            Match { scrutinee, arms } => {
                if idx == 0 { scrutinee } else {
                    let arm_off = idx - 1;
                    if arm_off % 2 != 1 {
                        return Err(lex_ast::TransformError::UnknownNode { at: at.into() });
                    }
                    let arm_index = arm_off / 2;
                    &arms.get(arm_index)
                        .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?
                        .body
                }
            }
            Block { statements, result } => {
                if idx < statements.len() { &statements[idx] }
                else if idx == statements.len() { result }
                else { return Err(lex_ast::TransformError::UnknownNode { at: at.into() }); }
            }
            Constructor { args, .. } | TupleLit { items: args, .. }
            | ListLit { items: args, .. } => args.get(idx)
                .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?,
            RecordLit { fields } => &fields.get(idx)
                .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?
                .value,
            FieldAccess { value, .. } if idx == 0 => value,
            Lambda { body, .. } if idx == 0 => body,
            BinOp { lhs, rhs, .. } => match idx {
                0 => lhs, 1 => rhs,
                _ => return Err(lex_ast::TransformError::UnknownNode { at: at.into() }),
            },
            UnaryOp { expr, .. } if idx == 0 => expr,
            Return { value } if idx == 0 => value,
            _ => return Err(lex_ast::TransformError::UnknownNode { at: at.into() }),
        };
    }
    let Let { name, .. } = current else {
        return Err(lex_ast::TransformError::NotALet {
            at: at.into(),
            found_kind: lex_cexpr_kind(current),
        });
    };
    Ok(name)
}

fn lex_cexpr_kind(e: &lex_ast::CExpr) -> &'static str {
    use lex_ast::CExpr::*;
    match e {
        Literal { .. } => "Literal", Var { .. } => "Var",
        Call { .. } => "Call", Let { .. } => "Let",
        Match { .. } => "Match", Block { .. } => "Block",
        Constructor { .. } => "Constructor", RecordLit { .. } => "RecordLit",
        TupleLit { .. } => "TupleLit", ListLit { .. } => "ListLit",
        FieldAccess { .. } => "FieldAccess", Lambda { .. } => "Lambda",
        BinOp { .. } => "BinOp", UnaryOp { .. } => "UnaryOp",
        Return { .. } => "Return",
    }
}

/// Extract the declared `[budget(N)]` integer from a stage's
/// effect set, if any (#280 + #247). Returns `None` for stages
/// that aren't `FnDecl` or don't carry a budget effect — same
/// shape as `lex_vcs::budget_from_effects`.
fn budget_of_stage(stage: &Stage) -> Option<u64> {
    let fd = match stage {
        Stage::FnDecl(fd) => fd,
        _ => return None,
    };
    let mut min_cost: Option<u64> = None;
    for eff in &fd.effects {
        if eff.name != "budget" { continue }
        if let Some(lex_ast::EffectArg::Int { value }) = &eff.arg {
            let n = *value as u64;
            min_cost = Some(min_cost.map(|c| c.min(n)).unwrap_or(n));
        }
    }
    min_cost
}

/// Serialize a stage to its canonical-JSON byte form. Used by
/// `publish_signed` for delta encoding (#261 slice 3) — both the
/// "compute the diff" path and the "write a full snapshot"
/// fallback need exactly the same bytes.
fn canonical_bytes(stage: &Stage) -> Result<Vec<u8>, StoreError> {
    let v = serde_json::to_value(stage)?;
    Ok(lex_ast::canon_json::to_canonical_string(&v).into_bytes())
}

#[allow(dead_code)]
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}
