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
        let meta_path = self.impl_dir(&sig).join(format!("{}.metadata.json", stage_id));

        if !ast_path.exists() {
            write_canonical_json(&ast_path, stage)?;
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
        let path = self.impl_dir(&sig).join(format!("{}.ast.json", stage_id));
        let bytes = fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes)?)
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
            return Err(StoreError::TypeError(errors));
        }
        // Persist the op without advancing the branch head — we want
        // the TypeCheck attestation visible *before* the gate runs,
        // so policies that require TypeCheck pass for newly-typed
        // ops. Then gate, then advance.
        let attestable = attestable_stage_ids(&transition);
        let op_effects = op_declared_effects(&op.kind);
        let new_head = self.persist_op_only(branch, op, transition)?;
        self.record_typecheck_passed(&attestable, &new_head.op_id)?;
        self.run_required_attestations_gate(&new_head.op_id, &attestable, &op_effects)?;
        self.set_branch_head_op(branch, new_head.op_id.clone())?;
        Ok(new_head.op_id)
    }

    /// Open the attestation log rooted at this store. The log lives
    /// under `<root>/attestations/`; opening is idempotent and cheap
    /// (`fs::create_dir_all`). Exposed publicly so consumers — `lex
    /// blame --with-evidence`, `GET /v1/stage/<id>/attestations` —
    /// can read what the store gate emitted without round-tripping
    /// through this crate's API surface.
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
        let new_head = self.persist_op_only(branch, op, transition)?;
        // The unchecked path doesn't auto-emit TypeCheck, so a
        // policy requiring TypeCheck (or any other attestation) on
        // ops with attestable stages will refuse to advance the
        // branch unless the caller emitted the attestation
        // separately first. This is the intended behavior — the
        // unchecked path opts out of the safety net the gate
        // depends on.
        self.run_required_attestations_gate(&new_head.op_id, &attestable, &op_effects)?;
        self.set_branch_head_op(branch, new_head.op_id.clone())?;
        Ok(new_head.op_id)
    }

    /// Persist an op via [`lex_vcs::apply`] but do NOT advance the
    /// branch head. Internal to the apply pipeline so
    /// `apply_operation` and `apply_operation_checked` share the
    /// pre-check + persist sequence without duplicating the
    /// validation code. The TypeCheck attestation and the
    /// `required_attestations` gate slot in between this and the
    /// final `set_branch_head_op` call.
    fn persist_op_only(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
    ) -> Result<lex_vcs::NewHead, StoreError> {
        // Pre-check: refuse to persist any op against a branch that
        // doesn't exist. Without this, applying against a non-default
        // ghost branch would write the op record (succeeding via
        // lex_vcs::apply on a None head) and only fail at
        // set_branch_head_op below — leaving an orphan op in the log
        // with no branch pointing at it.
        if branch != DEFAULT_BRANCH && self.get_branch(branch)?.is_none() {
            return Err(StoreError::UnknownBranch(branch.into()));
        }
        let log = lex_vcs::OpLog::open(self.root())?;
        let head_op = self.get_branch(branch)?.and_then(|b| b.head_op);
        lex_vcs::apply(&log, head_op.as_ref(), op, transition).map_err(|e| match e {
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
        op_id: &lex_vcs::OpId,
        stage_ids: &[String],
        op_effects: &std::collections::BTreeSet<String>,
    ) -> Result<(), StoreError> {
        // Build the candidate slice once and reuse it for both
        // gates. Ops with no attestable stage (imports, empty
        // merges) get a single `None`-stage tuple; both gates skip
        // those — there's nothing to attest.
        let candidate: Vec<(lex_vcs::OpId, Option<String>, std::collections::BTreeSet<String>)> =
            if stage_ids.is_empty() {
                vec![(op_id.clone(), None, op_effects.clone())]
            } else {
                stage_ids
                    .iter()
                    .map(|sid| (op_id.clone(), Some(sid.clone()), op_effects.clone()))
                    .collect()
            };
        let attest_log = self.attestation_log()?;

        // #248: producer-block gate. Always evaluated regardless of
        // policy.json contents — `ProducerBlock` attestations live
        // in the attestation log, not policy.json.
        crate::policy::check_producer_block(&attest_log, &candidate)
            .map_err(StoreError::ProducerBlocked)?;

        // #245: required-attestations gate. Only fires when the
        // policy declares rules.
        let policy = match crate::policy::load(self.root())? {
            Some(p) if !p.required_attestations.is_empty() => p,
            _ => return Ok(()),
        };
        crate::policy::check_required_attestations(&attest_log, &candidate, &policy)
            .map_err(StoreError::BranchAdvanceBlocked)
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

/// The set of stage_ids a transition introduces. These are the
/// stages a successful TypeCheck pass attests *about* — the new
/// head produced by Create/Replace, the renamed body, or the per-
/// sig resolution of a Merge. Removes and ImportOnly produce no
/// attestable stage; the program typechecks but no specific stage
/// is the subject of the claim.
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

#[allow(dead_code)]
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}
