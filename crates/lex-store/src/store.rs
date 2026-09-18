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
use std::collections::BTreeMap;
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
    /// A branch-head advance (e.g. the ref half of `op push`) was
    /// asked to move `branch` to `attempted`, but `attempted` is not a
    /// descendant of the branch's `current` head — a non-fast-forward
    /// that would orphan history. Refused, git-style, so a disjoint or
    /// diverged push can't silently clobber a shared branch. The op
    /// objects may already be present; only the ref is left unchanged.
    #[error("non-fast-forward on `{branch}`: {attempted} is not a descendant of current head {current}")]
    NonFastForward { branch: String, current: lex_vcs::OpId, attempted: lex_vcs::OpId },
    #[error("unknown blob `{0}`")]
    UnknownBlob(String),
    #[error("unknown blob ref `{namespace}/{key}`")]
    UnknownBlobRef { namespace: String, key: String },
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
    /// A dependency being resolved for the write-time gate (#930) is a
    /// multi-module package. Per-module signature extraction (picking the
    /// imported module's file out of the de-flattened tree) is not yet
    /// implemented; single-module (leaf) dependencies resolve today. A
    /// caller can treat this as "cannot resolve here" rather than a hard
    /// failure.
    #[error("multi-module dependency resolution is not yet supported")]
    UnsupportedMultiModuleDependency,
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
    #[error("session `{session_id}` budget exceeded: spent_after={spent_after} > cap={cap}")]
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

/// Everything a regenerator needs to *replay* an op (#836 G3), produced
/// by [`Store::replay_request`]. The model call is external: a harness
/// feeds `prompt` + `parent_program` to `model`, then hands the
/// regenerated stage to [`Store::replay_compare`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReplayRequest {
    pub op_id: String,
    /// The sig the op changed — the function to regenerate.
    pub target_sig: String,
    /// The target function's name (the recorded stage is a function
    /// for every replayable op).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    /// The target function's rendered signature (`fn name(...) -> T`),
    /// so a regenerator knows the interface to implement without
    /// re-deriving it from the sig hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_signature: Option<String>,
    /// The stage id a faithful regeneration should reproduce.
    pub expected_stage_id: String,
    /// The recorded intent prompt (`None` if the op carried no intent).
    pub prompt: Option<String>,
    /// The recorded model (`provider/name[@version]`), if any.
    pub model: Option<String>,
    /// The recorded session id, if any.
    pub session_id: Option<String>,
    /// The program the change was made against — the parent state
    /// rendered to source — the context a regenerator needs.
    pub parent_program: String,
}

/// The result of comparing a regenerated candidate against an op's
/// recorded output (#836 G3), returned by [`Store::replay_compare`]
/// after it emits the `Replay` attestation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReplayOutcome {
    pub op_id: String,
    pub expected_stage_id: String,
    /// The candidate's stage id when it regenerated the same sig, else
    /// `None`.
    pub produced_stage_id: Option<String>,
    /// Whether the regeneration reproduced the recorded change (exact or
    /// behavioral).
    pub reproduced: bool,
    /// Set when reproduction was behavioral (same values over N sampled
    /// inputs) rather than an exact stage-id match. `None` for an exact
    /// match or a genuine miss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavioral_samples: Option<usize>,
    /// The id of the `Replay` attestation this comparison emitted.
    pub attestation_id: String,
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

/// One line of `stage_index.jsonl`. See `Store::lookup_lifecycle`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StageIndexEntry {
    stage_id: String,
    sig_id: String,
}

/// Sentinel `sig_id` value recording "a full scan already established
/// this stage_id exists nowhere in the store" (#825). Never a real
/// sig — sig directory names are never empty.
const MISSING_STAGE_MARKER: &str = "";

pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open or create a store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("stages"))?;
        fs::create_dir_all(root.join("traces"))?;
        let store = Self { root };
        store.ensure_stage_index();
        Ok(store)
    }

    /// One-time migration for a store that predates the reverse
    /// index (#822), or whose previous rebuild pass never finished
    /// (e.g. the process was killed or its client disconnected
    /// mid-request — server-side work keeps running either way, but
    /// a *restart* genuinely stops it): build the index in a single
    /// pass instead of leaving every subsequent `lookup_lifecycle`
    /// call to discover its own entry via the slow per-call scan-
    /// and-backfill fallback.
    ///
    /// That per-call fallback is fine for the rare individual miss
    /// it was designed for, but pathological as a *bulk* cold-start
    /// strategy: on a tenant with a few thousand functions it means
    /// redoing an O(total sigs) scan from scratch for *each* of a
    /// few thousand cold entries — O(total sigs²) — which measured
    /// as a near-stall (page-cache thrashing) on a memory-
    /// constrained host. A single pass over `list_sigs()` is
    /// O(total sigs) total.
    ///
    /// Gated on a dedicated completion marker
    /// (`stage_index.complete`), NOT on `stage_index.jsonl`'s mere
    /// existence — a partially-built index file (left behind by an
    /// interrupted rebuild, lazy or bulk) must still trigger a
    /// re-run so the remaining entries get backfilled in one more
    /// cheap O(total sigs) pass, not silently be mistaken for
    /// "already done" and fall back to the slow per-call path for
    /// whatever's left. `rebuild_stage_index` already skips entries
    /// it finds present, so re-running it against a partial index
    /// only does the work that remains. The marker is written only
    /// after a full pass returns `Ok`, so a failed pass (e.g. an I/O
    /// error partway through `list_sigs`) is retried on the next
    /// open rather than being marked done.
    ///
    /// Runs once per `Store::open` call — which, in a long-lived
    /// server (lex-hub caches one `Store` per tenant for the life of
    /// the process), means once per tenant per process lifetime, not
    /// once per request. Once the marker exists (the steady state
    /// after the first successful run on any given host) this is a
    /// single cheap file-existence check. Best-effort like the rest
    /// of the index: any failure here just leaves the slower per-call
    /// fallback as the only path, never breaks correctness.
    fn ensure_stage_index(&self) {
        if self.stage_index_complete_marker_path().exists() {
            return;
        }
        if self.rebuild_stage_index().is_ok() {
            let _ = fs::write(self.stage_index_complete_marker_path(), "");
        }
    }

    fn stage_index_complete_marker_path(&self) -> PathBuf {
        self.root.join("stage_index.complete")
    }

    /// Build (or top up) the reverse index in one pass over every
    /// SigId in the store, rather than relying on `lookup_lifecycle`
    /// to discover entries one at a time. Safe to call at any time,
    /// including on a partially-built index (e.g. one left behind by
    /// an interrupted request that was populating it lazily): already-
    /// indexed stage_ids are skipped, so this only does the work that
    /// remains. Returns the number of newly-added entries.
    pub fn rebuild_stage_index(&self) -> Result<usize, StoreError> {
        // A sig's lifecycle can list the same stage_id more than once
        // (Draft, then later Active, then Deprecated all carry the
        // same stage_id with a different status) -- track newly-seen
        // keys locally too, not just what was already on disk at the
        // start, so a repeated stage_id within one sig's transitions
        // doesn't get appended to the index more than once.
        let mut existing = self.load_stage_index();
        let mut added = 0usize;
        for sig in self.list_sigs()? {
            let Ok(life) = self.read_lifecycle(&sig) else { continue };
            for t in &life.transitions {
                if !existing.contains_key(&t.stage_id) {
                    self.append_stage_index_entry(&t.stage_id, &sig);
                    existing.insert(t.stage_id.clone(), sig.clone());
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ── Generic content-addressed blobs (#5 / M6.1) ──────────────────────────
    //
    // The stage store holds typed Lex ASTs; loom-style artifacts (generated
    // code, JSON, prose) are opaque text. These blob methods give the store a
    // generic content-addressed object alongside stages, plus a lightweight
    // ref namespace so callers can bind names (e.g. a sprint's node ids) to
    // blob shas without touching the operation-log branch machinery.
    //
    // The sha is the lowercase hex SHA-256 of the content's UTF-8 bytes —
    // identical to Lex's `crypto.sha256_str`, so a blob written here and an
    // artifact content-addressed in loom's SQLite store share the same id and
    // are interchangeable by reference. Store-scoped, so under lex-hub each
    // tenant store gets its own blob space for free.

    fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    fn blob_refs_dir(&self) -> PathBuf {
        self.root.join("blobrefs")
    }

    /// Content-address `content` and persist it under `<root>/blobs/<sha>`.
    /// Returns the sha. Idempotent: re-putting identical content is a no-op.
    /// Concurrency-safe — writes to a unique temp file then atomically renames
    /// onto the content-addressed path, so parallel writers of the same content
    /// can't corrupt it.
    pub fn put_blob(&self, content: &str) -> Result<String, StoreError> {
        use sha2::{Digest, Sha256};
        let sha = hex::encode(Sha256::digest(content.as_bytes()));
        let dir = self.blobs_dir();
        let path = dir.join(&sha);
        if path.exists() {
            return Ok(sha);
        }
        fs::create_dir_all(&dir)?;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{sha}.{}.{n}.tmp", std::process::id()));
        fs::write(&tmp, content.as_bytes())?;
        // rename is atomic on the same filesystem; identical content makes a
        // last-writer-wins race harmless.
        fs::rename(&tmp, &path)?;
        Ok(sha)
    }

    /// Read a blob by its sha. `UnknownBlob` if absent.
    pub fn get_blob(&self, sha: &str) -> Result<String, StoreError> {
        match fs::read_to_string(self.blobs_dir().join(sha)) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::UnknownBlob(sha.to_string()))
            }
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Whether a blob with this sha exists.
    pub fn has_blob(&self, sha: &str) -> bool {
        self.blobs_dir().join(sha).exists()
    }

    /// Bind `key` to a blob `sha` within `namespace` (e.g. namespace
    /// `"loom/sprint-abc"`, key `"build-node"`). Overwrites an existing
    /// binding. The namespace may contain `/`; neither namespace nor key may
    /// contain a `..` path component.
    pub fn set_blob_ref(&self, namespace: &str, key: &str, sha: &str) -> Result<(), StoreError> {
        let dir = self.blob_ref_namespace_dir(namespace, key)?;
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(key), sha.as_bytes())?;
        Ok(())
    }

    /// Resolve `namespace`/`key` to a blob sha. `UnknownBlobRef` if unbound.
    pub fn get_blob_ref(&self, namespace: &str, key: &str) -> Result<String, StoreError> {
        let dir = self.blob_ref_namespace_dir(namespace, key)?;
        match fs::read_to_string(dir.join(key)) {
            Ok(s) => Ok(s.trim().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::UnknownBlobRef {
                namespace: namespace.to_string(),
                key: key.to_string(),
            }),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    /// Blob-ref namespace for committed lockfiles (#930 phase 2b-1).
    const LOCK_NS: &'static str = "lock";

    /// Record the `lex.lock` committed with the package head `head_op` — the
    /// exact dependency versions and op-log heads that head was built and
    /// type-checks against (#930 phase 2b-1: "HEAD + its committed lex.lock
    /// always type-checks"). Content-addressed via [`Self::put_blob`] and
    /// bound under the `lock` namespace keyed by the head op, so it is
    /// idempotent (a re-push converges) and travels with the package through
    /// the same object-sync path as stages and intents. Keyed by head op
    /// rather than by branch so re-verifying a *historical* head resolves it
    /// against the lock that head actually committed, not whatever the branch
    /// points at now.
    pub fn set_committed_lock(&self, head_op: &str, lock_toml: &str) -> Result<(), StoreError> {
        let sha = self.put_blob(lock_toml)?;
        self.set_blob_ref(Self::LOCK_NS, head_op, &sha)
    }

    /// The `lex.lock` committed with `head_op`, or `None` when the head
    /// carries no committed lock — a dependency-free package, or one
    /// published before locks were committed (the write-time gate then has no
    /// registry/git dependencies to resolve, exactly as today).
    pub fn committed_lock(&self, head_op: &str) -> Result<Option<String>, StoreError> {
        match self.get_blob_ref(Self::LOCK_NS, head_op) {
            Ok(sha) => Ok(Some(self.get_blob(&sha)?)),
            Err(StoreError::UnknownBlobRef { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// All `key → sha` bindings in a namespace (e.g. every artifact in a
    /// sprint). Empty map if the namespace has no bindings yet.
    pub fn list_blob_refs(
        &self,
        namespace: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        let dir = self.blob_ref_namespace_dir(namespace, "x")?;
        let mut out = std::collections::BTreeMap::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(StoreError::Io(e)),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let key = entry.file_name().to_string_lossy().to_string();
                let sha = fs::read_to_string(entry.path())?.trim().to_string();
                out.insert(key, sha);
            }
        }
        Ok(out)
    }

    // Resolve the on-disk dir for a (namespace, key), rejecting `..` traversal
    // and `/` in the key. `key` is validated but not joined here (callers join
    // it themselves so `list_blob_refs` can pass a dummy).
    fn blob_ref_namespace_dir(&self, namespace: &str, key: &str) -> Result<PathBuf, StoreError> {
        if key.contains('/') || key.contains('\\') || key.split('/').any(|c| c == "..") {
            return Err(StoreError::UnknownBlobRef {
                namespace: namespace.to_string(),
                key: key.to_string(),
            });
        }
        let mut dir = self.blob_refs_dir();
        for comp in namespace.split('/') {
            if comp == ".." || comp.contains('\\') {
                return Err(StoreError::UnknownBlobRef {
                    namespace: namespace.to_string(),
                    key: key.to_string(),
                });
            }
            if !comp.is_empty() {
                dir.push(comp);
            }
        }
        Ok(dir)
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn sig_dir(&self, sig: &str) -> PathBuf {
        self.root.join("stages").join(sig)
    }
    fn impl_dir(&self, sig: &str) -> PathBuf {
        self.sig_dir(sig).join("implementations")
    }
    fn tests_dir(&self, sig: &str) -> PathBuf {
        self.sig_dir(sig).join("tests")
    }
    fn specs_dir(&self, sig: &str) -> PathBuf {
        self.sig_dir(sig).join("specs")
    }
    fn lifecycle_path(&self, sig: &str) -> PathBuf {
        self.sig_dir(sig).join("lifecycle.json")
    }

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
        let meta_path = self
            .impl_dir(&sig)
            .join(format!("{}.metadata.json", stage_id));

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
            // Register the new stage_id's owning sig up front so a
            // later `lookup_lifecycle` (e.g. `get_ast`) never needs
            // to fall back to a full tenant-wide scan for it.
            self.append_stage_index_entry(&stage_id, &sig);
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
            return Err(StoreError::InvalidTransition(
                "tombstoned cannot be activated".into(),
            ));
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
        let cur = life
            .status_of(stage_id)
            .ok_or_else(|| StoreError::UnknownStage(stage_id.into()))?;
        if cur != StageStatus::Active {
            return Err(StoreError::InvalidTransition(format!(
                "{cur:?} ⇒ Deprecated"
            )));
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
        let cur = life
            .status_of(stage_id)
            .ok_or_else(|| StoreError::UnknownStage(stage_id.into()))?;
        if cur != StageStatus::Deprecated {
            return Err(StoreError::InvalidTransition(format!(
                "{cur:?} ⇒ Tombstone"
            )));
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
        let mut by_stage: indexmap::IndexMap<String, StageHistoryEntry> = indexmap::IndexMap::new();
        for t in &life.transitions {
            let entry = by_stage
                .entry(t.stage_id.clone())
                .or_insert(StageHistoryEntry {
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

    /// Bulk AST fetch for callers that already know each stage's
    /// **signature** — a branch head map, for instance, which is keyed
    /// by SigId and whose values are the StageIds it points at.
    ///
    /// Prefer this over [`Self::get_asts_bulk`] whenever the SigId is in
    /// hand, because resolving a StageId back to a SigId is not
    /// reliable: a StageId hashes the structural signature plus the
    /// implementation, deliberately *not* the name
    /// (`docs/INVARIANTS.md`), so two functions that differ only in name
    /// share one StageId while having two distinct SigIds — and two
    /// separate ASTs, one under each sig directory. `stage_index` maps
    /// each StageId to a single sig, so `get_ast`/`get_asts_bulk` return
    /// whichever of those ASTs the index happens to name, i.e. the wrong
    /// name half the time (#826). Reading straight from the sig the
    /// caller already knows removes the ambiguity — and skips loading
    /// the index at all.
    ///
    /// Returns results in the same order as `pairs`, `Err` for anything
    /// that fails to resolve (mirroring `get_ast`'s error semantics).
    pub fn get_asts_for_sigs_bulk(
        &self,
        pairs: &[(String, String)],
    ) -> Vec<Result<Stage, StoreError>> {
        pairs
            .iter()
            .map(|(sig_id, stage_id)| {
                let bytes = self.read_stage_canonical_bytes(sig_id, stage_id)?;
                Ok(serde_json::from_slice(&bytes)?)
            })
            .collect()
    }

    /// Bulk variant of [`Self::get_ast`] for callers resolving many
    /// stage_ids at once (e.g. `pkg_publish_handler`'s `old_head`
    /// scan over every live function in a tenant, once per publish
    /// request). `get_ast` in a loop calls `lookup_lifecycle` once
    /// per stage_id, and `lookup_lifecycle`'s index-hit path reads
    /// and re-parses the *entire* `stage_index.jsonl` on every single
    /// call — fine for one call, but O(index size × N) for N calls in
    /// a row, which dominates once the index itself is large (#825's
    /// follow-up: still correct and far better than the pre-index
    /// full-tenant-scan-per-call behavior, but the per-call reparse
    /// is itself a real, measured cost — 87.6s for 3,664 calls against
    /// a ~14k-line index on the alpibrusl tenant).
    ///
    /// This loads the index once for the whole batch and keeps it in
    /// memory across all `stage_ids`, only touching disk again to
    /// append genuinely new entries (a positive backfill or a
    /// negative "not found anywhere" cache, same as the single-call
    /// path) — never to re-read what's already loaded.
    ///
    /// Returns results in the same order as `stage_ids`, `Err` for
    /// anything that fails to resolve (mirroring `get_ast`'s error
    /// semantics per call).
    pub fn get_asts_bulk(&self, stage_ids: &[String]) -> Vec<Result<Stage, StoreError>> {
        let mut index = self.load_stage_index();
        let mut sigs_cache: BTreeMap<String, Option<Lifecycle>> = BTreeMap::new();
        let mut all_sigs: Option<Vec<String>> = None;

        stage_ids
            .iter()
            .map(|stage_id| {
                self.lookup_lifecycle_bulk(stage_id, &mut index, &mut sigs_cache, &mut all_sigs)
                    .and_then(|sig| {
                        let bytes = self.read_stage_canonical_bytes(&sig, stage_id)?;
                        Ok(serde_json::from_slice(&bytes)?)
                    })
            })
            .collect()
    }

    /// Shared implementation behind [`Self::get_asts_bulk`]: identical
    /// logic to [`Self::lookup_lifecycle`], but reads and writes the
    /// caller-supplied `index` map instead of reloading it from disk
    /// on every call, and memoizes `read_lifecycle` per sig and the
    /// `list_sigs()` full-scan list across the whole batch. Disk
    /// writes for newly-discovered entries (positive or negative)
    /// still happen immediately, same as the single-call path — only
    /// the repeated *reads* are batched away.
    fn lookup_lifecycle_bulk(
        &self,
        stage_id: &str,
        index: &mut BTreeMap<String, String>,
        sigs_cache: &mut BTreeMap<String, Option<Lifecycle>>,
        all_sigs: &mut Option<Vec<String>>,
    ) -> Result<String, StoreError> {
        if let Some(sig) = index.get(stage_id) {
            if sig == MISSING_STAGE_MARKER {
                return Err(StoreError::UnknownStage(stage_id.into()));
            }
            let life = sigs_cache
                .entry(sig.clone())
                .or_insert_with(|| self.read_lifecycle(sig).ok());
            if let Some(life) = life {
                if life.transitions.iter().any(|t| t.stage_id == stage_id) {
                    return Ok(sig.clone());
                }
            }
        }
        if all_sigs.is_none() {
            *all_sigs = Some(self.list_sigs()?);
        }
        for sig in all_sigs.as_ref().unwrap() {
            let life = sigs_cache
                .entry(sig.clone())
                .or_insert_with(|| self.read_lifecycle(sig).ok());
            if let Some(life) = life {
                if life.transitions.iter().any(|t| t.stage_id == stage_id) {
                    self.append_stage_index_entry(stage_id, sig);
                    index.insert(stage_id.to_string(), sig.clone());
                    return Ok(sig.clone());
                }
            }
        }
        self.append_stage_index_entry(stage_id, MISSING_STAGE_MARKER);
        index.insert(stage_id.to_string(), MISSING_STAGE_MARKER.to_string());
        Err(StoreError::UnknownStage(stage_id.into()))
    }

    /// Read the canonical bytes of a stage, walking back through
    /// any delta chain (#261 slice 3). The recursion ends at a
    /// `<stage_id>.ast.json` file (a full snapshot) or, in the
    /// degenerate case of a missing chain, with `UnknownStage`.
    fn read_stage_canonical_bytes(&self, sig: &str, stage_id: &str) -> Result<Vec<u8>, StoreError> {
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
        crate::delta::apply(&base_bytes, &delta).map_err(|e| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("applying delta for {stage_id}: {e}"),
            ))
        })
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
        if let Some((base_stage_id, base_chain_length)) = self.pick_delta_base(sig, stage_id)? {
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
        if let Some(parent) = ast_path.parent() {
            fs::create_dir_all(parent)?;
        }
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
        let Some(life) = life else {
            return Ok(None);
        };
        // Walk transitions newest-first; pick the first prior
        // stage that isn't this one and isn't tombstoned.
        let mut latest_per_stage: indexmap::IndexMap<&str, StageStatus> = indexmap::IndexMap::new();
        for t in &life.transitions {
            latest_per_stage.insert(&t.stage_id, t.to);
        }
        let mut candidates: Vec<&str> = latest_per_stage
            .iter()
            .filter(|(id, status)| **id != new_stage_id && **status != StageStatus::Tombstone)
            .map(|(id, _)| *id)
            .collect();
        // Reverse to get newest-first (transitions are append-only,
        // so latest_per_stage's iteration order matches insertion
        // order, oldest-first).
        candidates.reverse();
        let Some(&base) = candidates.first() else {
            return Ok(None);
        };
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
        let path = self
            .impl_dir(&sig)
            .join(format!("{}.metadata.json", stage_id));
        let bytes = fs::read(&path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn get_status(&self, stage_id: &str) -> Result<StageStatus, StoreError> {
        let (_sig, life) = self.lookup_lifecycle(stage_id)?;
        life.status_of(stage_id)
            .ok_or_else(|| StoreError::UnknownStage(stage_id.into()))
    }

    pub fn list_stages_by_name(&self, name: &str) -> Result<Vec<String>, StoreError> {
        // Walk every SigId → check metadata of any implementation; if its
        // name matches, include the SigId.
        let mut out = Vec::new();
        let stages_dir = self.root.join("stages");
        if !stages_dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&stages_dir)? {
            let entry = entry?;
            let sig_dir = entry.path();
            if !sig_dir.is_dir() {
                continue;
            }
            let sig = entry.file_name().to_string_lossy().to_string();
            // Look at any one metadata file under this SigId.
            let impls = self.impl_dir(&sig);
            if !impls.exists() {
                continue;
            }
            for f in fs::read_dir(impls)? {
                let f = f?;
                let p = f.path();
                if p.extension().is_some_and(|e| e == "json")
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().ends_with(".metadata.json"))
                {
                    if let Ok(bytes) = fs::read(&p) {
                        if let Ok(m) = serde_json::from_slice::<Metadata>(&bytes) {
                            if m.name == name {
                                if !out.contains(&sig) {
                                    out.push(sig.clone());
                                }
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
        if !stages_dir.exists() {
            return Ok(out);
        }
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
        if !dir.exists() {
            return Ok(Vec::new());
        }
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
        if !dir.exists() {
            return Ok(Vec::new());
        }
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

    // Native run-trace store — gated behind the `trace` feature (depends on
    // lex-trace). Off when a lower crate (lex-runtime) depends on lex-store to
    // avoid a dependency cycle; the blob/stage store below is unaffected.
    #[cfg(feature = "trace")]
    fn trace_path(&self, run_id: &str) -> PathBuf {
        self.root.join("traces").join(run_id).join("trace.json")
    }

    #[cfg(feature = "trace")]
    pub fn save_trace(&self, tree: &lex_trace::TraceTree) -> Result<String, StoreError> {
        let path = self.trace_path(&tree.run_id);
        write_canonical_json(&path, tree)?;
        Ok(tree.run_id.clone())
    }

    #[cfg(feature = "trace")]
    pub fn load_trace(&self, run_id: &str) -> Result<lex_trace::TraceTree, StoreError> {
        let bytes = fs::read(self.trace_path(run_id))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn list_traces(&self) -> Result<Vec<String>, StoreError> {
        let dir = self.root.join("traces");
        if !dir.exists() {
            return Ok(Vec::new());
        }
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

    /// `<root>/stage_index.jsonl` — an append-only, best-effort
    /// reverse index (`StageId` -> owning `SigId`), one JSON object
    /// per line. Backs `lookup_lifecycle`'s fast path; see its doc
    /// comment. Not a second source of truth: every entry is
    /// reconstructible from `stages/<sig>/lifecycle.json`, so a
    /// missing, truncated, or entirely absent index file only costs
    /// a slower lookup (the pre-existing full scan), never
    /// correctness — matching this module's "filesystem is the
    /// source of truth" stance (see the module doc comment) rather
    /// than introducing an actual second database.
    fn stage_index_path(&self) -> PathBuf {
        self.root.join("stage_index.jsonl")
    }

    /// Best-effort load of the whole reverse index into memory.
    /// Tolerates a missing file (no index yet) and a corrupt or
    /// torn last line (a crash mid-append under the single-writer
    /// Tier-1 assumption) by skipping lines that don't parse,
    /// rather than failing the lookup that triggered the load.
    fn load_stage_index(&self) -> std::collections::BTreeMap<String, String> {
        let mut out = std::collections::BTreeMap::new();
        let Ok(raw) = fs::read_to_string(self.stage_index_path()) else {
            return out;
        };
        for line in raw.lines() {
            if let Ok(entry) = serde_json::from_str::<StageIndexEntry>(line) {
                out.insert(entry.stage_id, entry.sig_id);
            }
        }
        out
    }

    /// Best-effort append of one new `(stage_id, sig_id)` pair.
    /// Failure (e.g. a read-only filesystem) only costs a future
    /// full scan for this stage_id, never correctness, so it's
    /// swallowed rather than propagated.
    fn append_stage_index_entry(&self, stage_id: &str, sig: &str) {
        use std::io::Write;
        let entry = StageIndexEntry { stage_id: stage_id.into(), sig_id: sig.into() };
        let Ok(line) = serde_json::to_string(&entry) else { return };
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.stage_index_path())
        {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Find which SigId owns a StageId, and that sig's lifecycle.
    ///
    /// Before the reverse index (#822): a full scan over *every*
    /// SigId in the tenant (`list_sigs()`, not scoped to the
    /// package being looked at), reading and parsing each one's
    /// `lifecycle.json` until a match turned up. `get_ast` — called
    /// once per pre-existing function when building a publish
    /// request's `old_fns_by_name` (`lex-api/src/handlers.rs`) —
    /// calls this once per function, so a tenant with a few thousand
    /// published functions turned a single publish into millions of
    /// individual file reads; measured at roughly an hour on the
    /// `alpibrusl` tenant's ~2,400-function store.
    ///
    /// Now: check the persisted reverse index first (one sequential
    /// file read instead of up to N separate ones). A miss — the
    /// index doesn't exist yet, or this stage_id predates it — falls
    /// back to the full scan and backfills the index so the next
    /// lookup for the same stage_id is fast.
    fn lookup_lifecycle(&self, stage_id: &str) -> Result<(String, Lifecycle), StoreError> {
        let index = self.load_stage_index();
        if let Some(sig) = index.get(stage_id) {
            if sig == MISSING_STAGE_MARKER {
                // A previous full scan already established this
                // stage_id exists nowhere in the store. Re-scanning
                // would find nothing again -- see #825: a genuinely
                // orphaned reference (e.g. from data predating some
                // store migration) is looked up on *every* call that
                // needs it, forever, so without this negative cache
                // it silently costs a full O(total sigs) scan each
                // time, indistinguishable from the positive case at
                // the call site. Measured directly: on the alpibrusl
                // tenant, 988 of 3,664 branch-head entries are
                // orphaned this way, turning one `pkg publish`'s
                // old_fns_by_name build into ~16M wasted lifecycle
                // reads.
                return Err(StoreError::UnknownStage(stage_id.into()));
            }
            if let Ok(life) = self.read_lifecycle(sig) {
                if life.transitions.iter().any(|t| t.stage_id == stage_id) {
                    return Ok((sig.clone(), life));
                }
            }
            // Index entry is stale or wrong (shouldn't happen in
            // practice — sig ownership of a stage_id is permanent).
            // Fall through to the full scan below rather than trust it.
        }
        for sig in self.list_sigs()? {
            if let Ok(life) = self.read_lifecycle(&sig) {
                if life.transitions.iter().any(|t| t.stage_id == stage_id) {
                    self.append_stage_index_entry(stage_id, &sig);
                    return Ok((sig, life));
                }
            }
        }
        // Genuinely not found anywhere: cache that fact so the next
        // lookup for this exact stage_id is an index hit, not another
        // full scan. Safe even if this stage_id somehow gets a real
        // sig later (content-addressed publish is idempotent, so
        // "later" only means "a byte-identical stage republished
        // under a real sig") — `append_stage_index_entry`'s later,
        // real entry is a later line in the file, and `load_stage_index`
        // folds duplicate keys last-write-wins, so the real entry wins.
        self.append_stage_index_entry(stage_id, MISSING_STAGE_MARKER);
        Err(StoreError::UnknownStage(stage_id.into()))
    }

    fn read_lifecycle(&self, sig: &str) -> Result<Lifecycle, StoreError> {
        let path = self.lifecycle_path(sig);
        if !path.exists() {
            return Ok(Lifecycle {
                sig_id: sig.into(),
                transitions: Vec::new(),
            });
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
        // Single-file / test callers don't publish a mangled package, so
        // there are no module prefixes to record (`in_file` stays `None`).
        self.publish_program_with_intent(
            branch,
            stages,
            diff,
            new_imports,
            activate,
            signer,
            None,
            &std::collections::BTreeMap::new(),
        )
    }

    /// [`Self::publish_program_signed`] plus an optional `intent_id`
    /// (#131 / #839): when given, every op this publish emits is stamped
    /// with it, so the op log records *why* the change happened — the
    /// prompt / model / session an agent was acting under — not only
    /// what it was. `lex recall --intent <id>` and `lex op replay` read
    /// it back. The caller records the [`lex_vcs::Intent`] in the
    /// [`lex_vcs::IntentLog`] beforehand; this only links ops to it.
    /// `None` is the existing (intent-less) behavior, so op ids for
    /// intent-less publishes are unchanged.
    // A batch publish legitimately takes the branch, program, diff,
    // imports, activate flag, signer, and now the intent — bundling
    // them into a struct for one optional field would obscure more
    // than it clarifies.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_program_with_intent(
        &self,
        branch: &str,
        stages: &[lex_ast::Stage],
        diff: &lex_vcs::DiffReport,
        new_imports: &lex_vcs::ImportMap,
        activate: bool,
        signer: Option<&lex_vcs::Keypair>,
        intent_id: Option<lex_vcs::IntentId>,
        // Mangling prefix → package source file, for a multi-module
        // package publish; empty for a single file. Recorded as each
        // `AddFunction`/`AddType`'s `in_file` so `export-git` can
        // de-flatten the package (#894).
        module_prefixes: &std::collections::BTreeMap<String, String>,
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

        // Build old-side views from the current branch. There used to be
        // an `old_name_to_sig: BTreeMap<String, SigId>` built here too,
        // keyed by bare function name — but a bare name is not unique
        // across a package's files (#818: two files can legitimately
        // both declare a local `validate` helper with different
        // signatures), so a name-keyed map silently collapsed distinct
        // SigIds onto one. `diff` now carries each entry's own resolved
        // `old_sig_id` directly (see `diff_report`'s doc comments), so
        // `diff_to_ops` no longer needs this lookup at all.
        let old_head = self.branch_head(branch)?;
        // Read every live function's effects through the SigId the head
        // names, in one batch. Two reasons, both load-bearing:
        //
        //   * Cost. This was a `get_ast` per live function, and
        //     `get_ast`'s index-hit path re-reads and re-parses the whole
        //     `stage_index.jsonl` on every call — O(index × live fns) per
        //     publish, paid again for every `publish_program` call a
        //     multi-file publish makes (#828; measured 34s for a no-op
        //     republish of a real 21-file package against only 698 live
        //     functions, nearly all of it here).
        //   * Correctness. A StageId is name-independent, so two live
        //     functions differing only in name share one and the index
        //     maps it to a single sig — resolving by StageId therefore
        //     attributed one function's effects to the *other* one's sig,
        //     the same ambiguity #826 fixed in `pkg_publish_handler`.
        let head_pairs: Vec<(String, String)> = old_head
            .iter()
            .map(|(sig, stage)| (sig.clone(), stage.clone()))
            .collect();
        let old_effects: BTreeMap<String, BTreeSet<String>> = head_pairs
            .iter()
            .zip(self.get_asts_for_sigs_bulk(&head_pairs))
            .filter_map(|((sig, _), ast)| match ast.ok()? {
                lex_ast::Stage::FnDecl(fd) => {
                    let s: BTreeSet<String> =
                        fd.effects.iter().map(|e| e.name.clone()).collect();
                    Some((sig.clone(), s))
                }
                _ => None,
            })
            .collect();
        let old_imports = self.derive_imports_from_oplog(branch)?;

        let op_kinds = lex_vcs::diff_to_ops(lex_vcs::DiffInputs {
            old_head: &old_head,
            old_effects: &old_effects,
            old_imports: &old_imports,
            new_stages: stages,
            new_imports,
            diff,
            module_prefixes,
        })
        .map_err(|e| StoreError::InvalidTransition(format!("diff_to_ops: {e}")))?;

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
            let op =
                lex_vcs::Operation::new(kind.clone(), head_now.into_iter().collect::<Vec<_>>());
            // #131 / #839: stamp the caller's intent so the op log records
            // why this change happened, not just what it was. The CAS
            // retry path preserves `intent_id` when it rebuilds the op.
            let op = match &intent_id {
                Some(id) => op.with_intent(id.clone()),
                None => op,
            };
            let op_id = self.apply_operation(branch, op, transition)?;
            self.record_typecheck_passed(&attestable, &op_id)?;
            ops_out.push(PublishOp {
                op_id: op_id.clone(),
                kind: serde_json::to_value(&kind).map_err(StoreError::Serde)?,
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
                AddImport { in_file, module, alias } => {
                    // The op omits the alias when it's the module's
                    // default (last path segment) to keep its OpId
                    // stable; rebuild it the same way on the way out.
                    let alias =
                        alias.unwrap_or_else(|| lex_vcs::default_import_alias(&module));
                    out.entry(in_file)
                        .or_default()
                        .insert(lex_vcs::ImportRef { reference: module, alias });
                }
                RemoveImport { in_file, module } => {
                    // Removal is keyed by reference (the op carries no
                    // alias), so drop any binding of that module.
                    if let Some(set) = out.get_mut(&in_file) {
                        set.retain(|ir| ir.reference != module);
                    }
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
    /// `apply_operation` accepting `Option<&[Stage]>` and silently
    /// skipping the gate on `None` is exactly the kind of
    /// "secretly opt-out" path #130 is trying to remove. The honest
    /// split: `apply_operation` for the one caller that already
    /// typechecked its input up front (`publish_program`),
    /// `apply_operation_checked` for callers holding the candidate,
    /// [`Self::apply_operation_gated`] for single-parent callers
    /// that hold only the transition (`/v1/patch`), and
    /// [`Self::apply_merge_op_gated`] for merge commits (#833).
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
            self.run_required_attestations_gate(branch, &new_head.op_id, &attestable, &op_effects)
        })
    }

    /// The program that would exist on `branch` after `transition`
    /// is applied: the branch head (snapshot-cached) with the
    /// transition replayed over it, every resulting `(sig, stage)`
    /// bulk-loaded. Exact for a **single-parent** transition — the
    /// candidate [`Self::apply_operation_gated`] wants. Not valid for
    /// a merge: a `StageTransition::Merge` records only the delta
    /// relative to dst, while the op-DAG replay that computes a
    /// merge's real head walks both parents (#833).
    pub fn candidate_program_for(
        &self,
        branch: &str,
        transition: &lex_vcs::StageTransition,
    ) -> Result<Vec<Stage>, StoreError> {
        let mut head = self.branch_head(branch)?;
        crate::branches::apply_transition(&mut head, transition);
        let pairs: Vec<(String, String)> = head.into_iter().collect();
        self.get_asts_for_sigs_bulk(&pairs).into_iter().collect()
    }

    /// [`Self::apply_operation_checked`] for a **single-parent** op
    /// where the caller holds only the transition: assembles the
    /// candidate via [`Self::candidate_program_for`] and runs the
    /// gate. Same rejection semantics — `TypeError`, a `RepairHint`
    /// attestation, head unchanged, nothing persisted. This is the
    /// write path for `/v1/patch` (#833). Merge ops must not use it
    /// (see `candidate_program_for`); they go through
    /// [`Self::apply_merge_op_gated`].
    pub fn apply_operation_gated(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
    ) -> Result<lex_vcs::OpId, StoreError> {
        debug_assert!(
            op.parents.len() <= 1,
            "apply_operation_gated is single-parent only; merges use apply_merge_op_gated"
        );
        let candidate = self.candidate_program_for(branch, &transition)?;
        self.apply_operation_checked(branch, op, transition, &candidate)
    }

    /// The gated write path for **merge** commits (`commit_merge`,
    /// `POST /v1/merge/<id>/commit`, `lex merge commit`).
    ///
    /// A `StageTransition::Merge` records only the delta relative to
    /// dst; the sig->stage map every consumer reads is recomputed by
    /// replaying the op DAG, which for a merge walks *both* parents
    /// and can surface sigs the delta never mentions. So the only way
    /// to know the true post-merge program is to replay it — land the
    /// op and read `branch_head`. This lands the merge op,
    /// type-checks the resulting head, and on a failure rolls the
    /// head back and returns `TypeError`.
    ///
    /// Before #833 the merge paths landed through the ungated
    /// `apply_operation`, so a merge whose result didn't compose
    /// (e.g. dst still calls `helper`, an agent-supplied resolution
    /// dropped it) advanced the head with nothing to catch it.
    ///
    /// Rollback leaves the rejected merge op as an unreachable record
    /// (reclaimed by `lex op gc`, the same orphan crash-recovery
    /// already tolerates). A stage the merge names that was never
    /// published surfaces as the underlying `StoreError` from the
    /// bulk read — the "never advance onto content that can't be
    /// loaded" invariant from the other side.
    pub fn apply_merge_op_gated(
        &self,
        branch: &str,
        op: lex_vcs::Operation,
        transition: lex_vcs::StageTransition,
    ) -> Result<lex_vcs::OpId, StoreError> {
        let head_before = self.get_branch(branch)?.and_then(|b| b.head_op);
        // Capture the stages this merge introduces before `transition`
        // is moved into `apply_operation`; used for the TypeCheck
        // attestation below.
        let attestable = attestable_stage_ids(&transition);
        let op_id = self.apply_operation(branch, op, transition)?;

        let verdict = (|| -> Result<(), StoreError> {
            let head = self.branch_head(branch)?;
            let pairs: Vec<(String, String)> = head.into_iter().collect();
            let stages: Vec<Stage> =
                self.get_asts_for_sigs_bulk(&pairs).into_iter().collect::<Result<_, _>>()?;
            if let Err(errors) = lex_types::check_program(&stages) {
                return Err(StoreError::TypeError(errors));
            }
            Ok(())
        })();

        if let Err(e) = verdict {
            // Roll the head back. The empty-dst case never reaches
            // here (it fast-forwards without a merge op), so
            // `head_before` is always `Some` on this arm.
            if let Some(prev) = head_before {
                self.set_branch_head_op(branch, prev)?;
            }
            return Err(e);
        }
        // #835: the merge's post-merge head type-checked, but until now
        // that verdict left no trace in the attestation log — so a
        // merged stage looked un-type-checked to `lex blame
        // --with-evidence` and the attestation queries, unlike a
        // published or patched stage. Emit `TypeCheck::Passed` for the
        // stages the merge introduced, mirroring the publish / patch
        // paths (`record_typecheck_passed`). Emitted only after the
        // check passes and the head is committed, so a rolled-back
        // merge records nothing.
        self.record_typecheck_passed(&attestable, &op_id)?;
        Ok(op_id)
    }

    /// Type-check the program that would result from overlaying a merge
    /// `delta` onto `branch`'s current head — **without moving the
    /// head** (#834). `delta` maps `sig_id -> Some(stage)` to set that
    /// sig to `stage`, or `sig_id -> None` to remove it, exactly the
    /// `entries` a `StageTransition::Merge` records.
    ///
    /// This is the read-only, resolve-time counterpart of
    /// `apply_merge_op_gated`'s commit-time gate: it lets a merge
    /// session tell an agent *which resolution broke type-checking* the
    /// moment it is submitted, instead of only after a failed commit.
    /// `Ok(())` means the projected program composes; a type failure is
    /// `Err(StoreError::TypeError(..))`; a read failure is the
    /// corresponding `StoreError` I/O variant.
    pub fn typecheck_merge_projection(
        &self,
        branch: &str,
        delta: &std::collections::BTreeMap<String, Option<String>>,
    ) -> Result<(), StoreError> {
        let mut head = self.branch_head(branch)?;
        for (sig, stage) in delta {
            match stage {
                Some(s) => { head.insert(sig.clone(), s.clone()); }
                None => { head.remove(sig); }
            }
        }
        let pairs: Vec<(String, String)> = head.into_iter().collect();
        let stages: Vec<Stage> =
            self.get_asts_for_sigs_bulk(&pairs).into_iter().collect::<Result<_, _>>()?;
        if let Err(errors) = lex_types::check_program(&stages) {
            return Err(StoreError::TypeError(errors));
        }
        Ok(())
    }

    /// #838: attempt a typed three-way merge of a single sig's body for
    /// a `ModifyModify` conflict — the intra-function, better-than-git
    /// case where two agents edited *disjoint* subtrees of the same
    /// function (different match arms, different let bindings).
    ///
    /// `base` / `ours` (the dst side) / `theirs` (the src side) are the
    /// three stage ids the merge engine surfaced for `sig_id`. Loads
    /// the three `FnDecl`s, structurally merges the bodies
    /// ([`lex_vcs::merge_bodies`]), and accepts the result *only if* the
    /// merged function also type-checks against `dst_branch`'s head — a
    /// body that composes syntactically but not by type is still a
    /// conflict (#838). On success the merged stage is published
    /// (content-addressed, idempotent; orphaned and GC-reclaimable if
    /// the merge is never committed) and its id returned; `None` means
    /// "fall back to a whole-function conflict."
    ///
    /// Deliberately narrow for this slice: only pure body divergence is
    /// merged. If the two sides disagree on anything but the body
    /// (examples, type params — the signature is identical by
    /// construction, since all three share `sig_id`), or either stage
    /// isn't a function, it falls back to a conflict.
    pub fn try_semantic_body_merge(
        &self,
        dst_branch: &str,
        sig_id: &str,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<Option<String>, StoreError> {
        use lex_ast::Stage::FnDecl;
        let (base_fd, ours_fd, theirs_fd) =
            match (self.get_ast(base), self.get_ast(ours), self.get_ast(theirs)) {
                (Ok(FnDecl(b)), Ok(FnDecl(o)), Ok(FnDecl(t))) => (b, o, t),
                // A non-function stage (type decl / import) or a stage
                // that can't be loaded isn't an intra-body merge.
                _ => return Ok(None),
            };

        // Only the body may diverge between the two sides.
        if !fndecl_same_except_body(&ours_fd, &theirs_fd) {
            return Ok(None);
        }

        let merged_body =
            match lex_vcs::merge_bodies(&base_fd.body, &ours_fd.body, &theirs_fd.body) {
                lex_vcs::BodyMerge::Merged(b) => b,
                lex_vcs::BodyMerge::Conflict => return Ok(None),
            };

        let mut merged_fd = ours_fd.clone();
        merged_fd.body = merged_body;
        let merged_stage = lex_ast::Stage::FnDecl(merged_fd);
        let new_stage_id = match stage_id(&merged_stage) {
            Some(id) => id,
            None => return Ok(None),
        };

        // Type-check the merged fn in context: dst's head with this sig
        // swapped to the merged stage. Requires the merged stage to be
        // loadable, so publish first (idempotent, content-addressed).
        self.publish(&merged_stage)?;
        let mut delta = std::collections::BTreeMap::new();
        delta.insert(sig_id.to_string(), Some(new_stage_id.clone()));
        match self.typecheck_merge_projection(dst_branch, &delta) {
            Ok(()) => Ok(Some(new_stage_id)),
            // Composes syntactically, not by type → still a conflict.
            Err(StoreError::TypeError(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// #836 G3: assemble everything a regenerator needs to *replay* an
    /// op — re-derive the change from its recorded cause. Returns the
    /// op's recorded intent (prompt / model / session), the target sig
    /// and the stage id it produced, and the program the change was
    /// made against (the parent state, rendered to source). An external
    /// harness feeds the prompt + parent program to the recorded model,
    /// then hands the regenerated stage back to [`Self::replay_compare`]
    /// (lex owns the deterministic comparison; the model call is the
    /// harness's, matching the rest of the architecture).
    ///
    /// Errors with `UnknownOp` if the op_id is unknown, or
    /// `InvalidTransition` if the op didn't produce a stage (a removal /
    /// import / merge has nothing to regenerate).
    pub fn replay_request(&self, op_id: &str) -> Result<ReplayRequest, StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let record = log
            .get(&op_id.to_string())?
            .ok_or_else(|| StoreError::UnknownOp(op_id.to_string()))?;
        let (target_sig, expected_stage_id) = produced_sig_stage(&record.produces)
            .ok_or_else(|| StoreError::InvalidTransition(format!("op {op_id} produced no stage to replay")))?;

        let (prompt, model, session_id) = match &record.op.intent_id {
            Some(id) => {
                let intents = lex_vcs::IntentLog::open(self.root())?;
                match intents.get(id)? {
                    Some(i) => (Some(i.prompt), Some(model_label(&i.model)), Some(i.session_id)),
                    None => (None, None, None),
                }
            }
            None => (None, None, None),
        };

        // The program the op was applied against: the head state at its
        // (first) parent, rendered with the canonical printer. A root
        // op has no parent → empty program.
        let parent_program = match record.op.parents.first() {
            Some(parent) => self.program_source_at_op(parent)?,
            None => String::new(),
        };

        // The target function's name + signature, from the recorded
        // stage — a regenerator needs the interface, not just the hash.
        let (target_name, target_signature) = match self.get_ast(&expected_stage_id) {
            Ok(lex_ast::Stage::FnDecl(fd)) => {
                (Some(fd.name.clone()), Some(lex_vcs::render_signature(&fd)))
            }
            _ => (None, None),
        };

        Ok(ReplayRequest {
            op_id: op_id.to_string(),
            target_sig,
            target_name,
            target_signature,
            expected_stage_id,
            prompt,
            model,
            session_id,
            parent_program,
        })
    }

    /// #836 G3: compare a regenerated `candidate` against what the op
    /// recorded producing, and emit the `Replay` attestation. The
    /// reproducibility claim made concrete — a faithful regeneration of
    /// the same function from the same cause yields the same
    /// content-addressed stage id.
    ///
    /// `reproduced` is true iff the candidate is the same sig *and* the
    /// same stage id the op recorded. A candidate for a different sig
    /// counts as "not reproduced" (`produced_stage_id: None`) rather
    /// than an error — it's a legitimate, if negative, replay result.
    /// The attestation is addressed to the op's recorded stage, so
    /// `list_for_stage` surfaces it alongside the TypeCheck/Examples
    /// evidence.
    pub fn replay_compare(
        &self,
        op_id: &str,
        candidate: &Stage,
    ) -> Result<ReplayOutcome, StoreError> {
        let (target_sig, expected_stage_id) = self.replay_target(op_id)?;
        let cand_sig = lex_ast::sig_id(candidate);
        let cand_stage = stage_id(candidate);
        let produced_stage_id = match (cand_sig.as_deref(), &cand_stage) {
            // Same function regenerated: the produced stage is
            // whatever it content-addresses to.
            (Some(s), Some(st)) if s == target_sig => Some(st.clone()),
            // A different sig (or an unhashable stage) isn't a
            // regeneration of this op's change.
            _ => None,
        };
        let reproduced = produced_stage_id.as_deref() == Some(expected_stage_id.as_str());
        let detail = if reproduced {
            None
        } else {
            Some("regeneration did not reproduce the recorded stage".to_string())
        };
        self.emit_replay(op_id, &expected_stage_id, produced_stage_id, reproduced, None, detail)
    }

    /// Record a *negative* replay result for a regeneration that never
    /// yielded a comparable stage — the output didn't parse, or didn't
    /// define the target sig (#836 G3). Emits a `Replay { reproduced:
    /// false, produced_stage_id: None }` attestation with `reason` in
    /// its `Failed` detail, so an automated `lex op replay` run always
    /// records a verdict rather than aborting. `reason` is caller-supplied
    /// (e.g. "regenerated source did not parse").
    pub fn replay_record_miss(&self, op_id: &str, reason: &str) -> Result<ReplayOutcome, StoreError> {
        let (_target_sig, expected_stage_id) = self.replay_target(op_id)?;
        self.emit_replay(op_id, &expected_stage_id, None, false, None, Some(reason.to_string()))
    }

    /// `(target_sig, expected_stage_id)` for a replayable op, or an
    /// error if the op is unknown or produced no stage.
    fn replay_target(&self, op_id: &str) -> Result<(String, String), StoreError> {
        let log = lex_vcs::OpLog::open(self.root())?;
        let record = log
            .get(&op_id.to_string())?
            .ok_or_else(|| StoreError::UnknownOp(op_id.to_string()))?;
        produced_sig_stage(&record.produces)
            .ok_or_else(|| StoreError::InvalidTransition(format!("op {op_id} produced no stage to replay")))
    }

    /// Record a replay verdict the caller has already decided — used by
    /// the CLI's behavioral tier, which does the (VM-backed) equivalence
    /// check the store deliberately can't. `expected_stage_id` is looked
    /// up from the op. Set `behavioral_samples` to `Some(n)` when the
    /// candidate reproduced *behaviorally* over `n` sampled inputs rather
    /// than by exact stage-id match; the attestation then records that
    /// weaker-but-real claim distinctly.
    pub fn replay_record(
        &self,
        op_id: &str,
        produced_stage_id: Option<String>,
        reproduced: bool,
        behavioral_samples: Option<usize>,
        fail_detail: Option<String>,
    ) -> Result<ReplayOutcome, StoreError> {
        let (_target_sig, expected_stage_id) = self.replay_target(op_id)?;
        self.emit_replay(op_id, &expected_stage_id, produced_stage_id, reproduced, behavioral_samples, fail_detail)
    }

    /// Compute the exact-match verdict for a candidate *without* emitting
    /// an attestation — `(expected_stage_id, produced_stage_id, exact)`.
    /// Lets a caller (the CLI) fall back to a behavioral check on a valid
    /// but non-identical candidate and emit a single verdict, instead of
    /// [`Self::replay_compare`]'s emit-immediately shape.
    pub fn replay_stage_of(
        &self,
        op_id: &str,
        candidate: &Stage,
    ) -> Result<(String, Option<String>, bool), StoreError> {
        let (target_sig, expected_stage_id) = self.replay_target(op_id)?;
        let cand_sig = lex_ast::sig_id(candidate);
        let cand_stage = stage_id(candidate);
        let produced_stage_id = match (cand_sig.as_deref(), &cand_stage) {
            (Some(s), Some(st)) if s == target_sig => Some(st.clone()),
            _ => None,
        };
        let exact = produced_stage_id.as_deref() == Some(expected_stage_id.as_str());
        Ok((expected_stage_id, produced_stage_id, exact))
    }

    /// Emit the `Replay` attestation and build the outcome. Shared by
    /// [`Self::replay_compare`], [`Self::replay_record_miss`], and
    /// [`Self::replay_record`].
    fn emit_replay(
        &self,
        op_id: &str,
        expected_stage_id: &str,
        produced_stage_id: Option<String>,
        reproduced: bool,
        behavioral_samples: Option<usize>,
        fail_detail: Option<String>,
    ) -> Result<ReplayOutcome, StoreError> {
        let model = {
            let log = lex_vcs::OpLog::open(self.root())?;
            match log.get(&op_id.to_string())?.and_then(|r| r.op.intent_id) {
                Some(id) => lex_vcs::IntentLog::open(self.root())?
                    .get(&id)?
                    .map(|i| model_label(&i.model)),
                None => None,
            }
        };
        let result = if reproduced {
            lex_vcs::AttestationResult::Passed
        } else {
            lex_vcs::AttestationResult::Failed {
                detail: fail_detail.unwrap_or_else(|| "not reproduced".into()),
            }
        };
        let attestation = lex_vcs::Attestation::new(
            expected_stage_id.to_string(),
            Some(op_id.to_string()),
            None,
            lex_vcs::AttestationKind::Replay {
                expected_stage_id: expected_stage_id.to_string(),
                produced_stage_id: produced_stage_id.clone(),
                reproduced,
                behavioral_samples,
                model,
            },
            result,
            replay_producer(),
            None,
        );
        let attestation_id = attestation.attestation_id.clone();
        self.attestation_log()?.put(&attestation)?;
        Ok(ReplayOutcome {
            op_id: op_id.to_string(),
            expected_stage_id: expected_stage_id.to_string(),
            produced_stage_id,
            reproduced,
            behavioral_samples,
            attestation_id,
        })
    }

    /// The program at an op (that op and all its ancestors applied), as
    /// canonical stages. The behavioral replay tier needs the whole
    /// program — a regenerated function may call helpers from its parent
    /// state, so it can only be run in context. Exposed for the CLI's
    /// equivalence check; `op_id` may be any op in the log.
    pub fn program_stages_at_op(&self, op_id: &str) -> Result<Vec<Stage>, StoreError> {
        let oid: lex_vcs::OpId = op_id.to_string();
        let log = lex_vcs::OpLog::open(self.root())?;
        let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        for rec in log.walk_forward(&oid, None)? {
            crate::branches::apply_transition(&mut map, &rec.produces);
        }
        let pairs: Vec<(String, String)> = map.into_iter().collect();
        let stages: Vec<Stage> =
            self.get_asts_for_sigs_bulk(&pairs).into_iter().collect::<Result<_, _>>()?;
        Ok(stages)
    }

    /// The program at an op, rendered to source. Used to give a replay
    /// regenerator the context the change was made against.
    fn program_source_at_op(&self, op_id: &lex_vcs::OpId) -> Result<String, StoreError> {
        Ok(lex_ast::print_stages(&self.program_stages_at_op(op_id)?))
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
        let mut from_tool: Vec<&lex_vcs::Attestation> = all
            .iter()
            .filter(|a| a.produced_by.tool == tool_id)
            // Ignore self-referential trust attestations (we're
            // scoring evidence, not previous trust statements).
            .filter(|a| {
                !matches!(
                    a.kind,
                    lex_vcs::AttestationKind::ProducerTrust { .. }
                        | lex_vcs::AttestationKind::TrustWaived { .. }
                )
            })
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
        let head_op = self
            .list_branches()?
            .into_iter()
            .find_map(|b| self.get_branch(&b).ok().flatten().and_then(|x| x.head_op))
            .unwrap_or_else(|| "fresh".into());
        let evidence = format!(
            "window={window}, sample={}, head_op={head_op:.16}",
            from_tool.len()
        );
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

    /// The latest live `ProducerTrust` score (thousandths, `0..=1000`) for
    /// every producer that currently has trust: the newest score per tool by
    /// timestamp, excluding any tool under an active `ProducerBlock` (a block
    /// is a hard veto over trust, matching `recompute_producer_trust`).
    ///
    /// Used to export a capsule trusted-keys keyring from *earned* trust — the
    /// producer id doubles as the publisher's signing key downstream, so this
    /// turns track record into the allowlist `capsule install` consumes.
    pub fn live_producer_trust_scores(
        &self,
    ) -> Result<std::collections::BTreeMap<String, u32>, StoreError> {
        let log = self.attestation_log()?;
        let all = log.list_all()?;
        // Newest score per tool.
        let mut latest: std::collections::BTreeMap<String, (u64, u32)> =
            std::collections::BTreeMap::new();
        for a in &all {
            if let lex_vcs::AttestationKind::ProducerTrust {
                tool_id,
                score_thousandths,
                ..
            } = &a.kind
            {
                let entry = latest.entry(tool_id.clone()).or_insert((0, 0));
                if a.timestamp >= entry.0 {
                    *entry = (a.timestamp, *score_thousandths);
                }
            }
        }
        // Drop blocked producers; a block vetoes trust.
        let mut scores = std::collections::BTreeMap::new();
        for (tool, (_, score)) in latest {
            if lex_vcs::active_producer_block(&all, &tool).is_some() {
                continue;
            }
            scores.insert(tool, score);
        }
        Ok(scores)
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

    /// The hosted CI runner (#93): independently re-run the write-time
    /// type-check gate on a branch head and record the verdict as a
    /// `lex-hub-ci`-produced `TypeCheck` attestation for the stages the
    /// advance introduced. Called after an `op push` fast-forwards the
    /// head, so `require-attestation type_check` gates are backed by a
    /// producer that actually verified the code server-side, not by
    /// whatever attestation a client chose to attach. Does NOT move or
    /// roll back the head — the client's own always-valid-HEAD gate is
    /// what refuses a bad publish; this produces the trusted verdict on
    /// top of an already-committed advance (so a client that bypassed
    /// its gate is caught by a `TypeCheck::Failed` from `lex-hub-ci`).
    ///
    /// `from_head` is the branch head *before* the advance; the ops
    /// between it and `to_head` are the ones whose stages get attested.
    /// Idempotent: attestations are content-addressed, so re-verifying
    /// the same head is a no-op.
    pub fn verify_head_and_attest(
        &self,
        branch: &str,
        from_head: Option<&str>,
        to_head: &str,
    ) -> Result<HubCiVerdict, StoreError> {
        // Reconstruct the program at the new head and re-check it.
        let head = self.branch_head(branch)?;
        let pairs: Vec<(String, String)> = head.into_iter().collect();
        let stages: Vec<Stage> =
            self.get_asts_for_sigs_bulk(&pairs).into_iter().collect::<Result<_, _>>()?;
        let checked_stages = stages.len();
        let result = match lex_types::check_program(&stages) {
            Ok(_) => lex_vcs::AttestationResult::Passed,
            Err(errors) => lex_vcs::AttestationResult::Failed {
                detail: serde_json::to_string(&errors).unwrap_or_else(|_| "type errors".into()),
            },
        };
        let passed = matches!(result, lex_vcs::AttestationResult::Passed);

        // Stages introduced by THIS advance (from_head exclusive → to_head).
        let log = lex_vcs::OpLog::open(self.root())?;
        let to = to_head.to_string();
        let records = match from_head {
            Some(f) => log
                .walk_forward_since(&to, &f.to_string())?
                .unwrap_or_else(|| log.walk_forward(&to, None).unwrap_or_default()),
            None => log.walk_forward(&to, None)?,
        };
        let mut introduced: Vec<String> = Vec::new();
        for rec in &records {
            introduced.extend(attestable_stage_ids(&rec.produces));
        }

        let alog = self.attestation_log()?;
        for sid in &introduced {
            let att = lex_vcs::Attestation::new(
                sid.clone(),
                Some(to_head.to_string()),
                None,
                lex_vcs::AttestationKind::TypeCheck,
                result.clone(),
                hub_ci_producer(),
                None,
            );
            alog.put(&att)?;
        }

        let detail = match &result {
            lex_vcs::AttestationResult::Failed { detail } => Some(detail.clone()),
            _ => None,
        };
        Ok(HubCiVerdict { passed, checked_stages, attested_stages: introduced.len(), detail })
    }

    /// Emit an `Examples::Passed` attestation for a published stage
    /// whose behavioral `examples {}` block was run and passed (#835,
    /// Tier 1). Mirrors [`Self::record_typecheck_passed`]. The
    /// behavioral run itself happens one layer up (lex-api / lex-cli)
    /// because it needs the bytecode compiler + VM, which this crate
    /// deliberately doesn't depend on; the store only records the
    /// verdict. `file_hash` uses the stage id — the stage fully
    /// determines its own examples.
    pub fn record_examples_passed(
        &self,
        stage_id: &str,
        op_id: &lex_vcs::OpId,
        count: usize,
    ) -> Result<(), StoreError> {
        let log = self.attestation_log()?;
        let attestation = lex_vcs::Attestation::new(
            stage_id.to_string(),
            Some(op_id.clone()),
            None,
            lex_vcs::AttestationKind::Examples { file_hash: stage_id.to_string(), count },
            lex_vcs::AttestationResult::Passed,
            examples_producer(),
            None,
        );
        log.put(&attestation)?;
        Ok(())
    }

    /// Record a structured `Review` verdict on a stage (#836 G4).
    /// The verdict maps onto the attestation `result` so existing
    /// result-based tooling reads it: Approve->Passed,
    /// Reject->Failed, RequestChanges->Inconclusive.
    pub fn record_review(
        &self,
        stage_id: &str,
        op_id: Option<lex_vcs::OpId>,
        reviewer: &str,
        verdict: lex_vcs::ReviewVerdict,
        notes: Option<String>,
    ) -> Result<lex_vcs::AttestationId, StoreError> {
        let result = match verdict {
            lex_vcs::ReviewVerdict::Approve => lex_vcs::AttestationResult::Passed,
            lex_vcs::ReviewVerdict::Reject => lex_vcs::AttestationResult::Failed {
                detail: notes.clone().unwrap_or_else(|| "rejected".into()),
            },
            lex_vcs::ReviewVerdict::RequestChanges => lex_vcs::AttestationResult::Inconclusive {
                detail: notes.clone().unwrap_or_else(|| "changes requested".into()),
            },
        };
        let att = lex_vcs::Attestation::new(
            stage_id.to_string(),
            op_id,
            None,
            lex_vcs::AttestationKind::Review { reviewer: reviewer.to_string(), verdict, notes },
            result,
            review_producer(reviewer),
            None,
        );
        let id = att.attestation_id.clone();
        self.attestation_log()?.put(&att)?;
        Ok(id)
    }

    /// The latest `Review` verdict recorded on a stage, if any
    /// (#836 G4). "Latest" is by attestation timestamp; ties keep the
    /// last one seen. Used by `promote_candidate` to honor a standing
    /// Reject.
    pub fn latest_review_verdict(
        &self,
        stage_id: &str,
    ) -> Result<Option<lex_vcs::ReviewVerdict>, StoreError> {
        let log = self.attestation_log()?;
        let mut latest: Option<(u64, lex_vcs::ReviewVerdict)> = None;
        for a in log.list_for_stage(&stage_id.to_string())? {
            if let lex_vcs::AttestationKind::Review { verdict, .. } = a.kind {
                if latest.as_ref().map(|(t, _)| a.timestamp >= *t).unwrap_or(true) {
                    latest = Some((a.timestamp, verdict));
                }
            }
        }
        Ok(latest.map(|(_, v)| v))
    }

    /// Consult `policy.session_budgets` for the op's session
    /// (resolved via `op.intent_id → Intent.session_id`) and
    /// refuse if applying would push the session's monotonic spend
    /// over the configured cap (#292 slice 3).
    ///
    /// Ops without an `intent_id`, or whose intent has no
    /// configured cap, return Ok without any disk read.
    fn check_session_budget(&self, op: &lex_vcs::Operation) -> Result<(), StoreError> {
        let Some(intent_id) = op.intent_id.as_deref() else {
            return Ok(());
        };
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
        let errors_json = serde_json::to_value(errors).map_err(StoreError::Serde)?;
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
                None, // the failed op was never persisted; not the
                // attestation's op_id (which is for a
                // *successful* op).
                None,
                lex_vcs::AttestationKind::RepairHint {
                    failed_op_id: failed_op_id.clone(),
                    errors: errors_json.clone(),
                    suggested_transform: suggested_transform.clone(),
                },
                lex_vcs::AttestationResult::Failed {
                    detail: format!(
                        "op {} rejected: {} type error(s)",
                        failed_op_id,
                        errors.len()
                    ),
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
        let rec = log
            .get(op_id)?
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
                run_id,
                root_target,
                &rec.op_id,
                result.clone(),
                producer.clone(),
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
        let new_stage = lex_ast::replace_match_arm(&from_stage, match_node, arm_index, new_body)
            .map_err(StoreError::TransformError)?;
        let sig = lex_ast::sig_id(&from_stage).ok_or(StoreError::CannotPublishImport)?;
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
        let op = lex_vcs::Operation::new(kind, head_now.into_iter().collect::<Vec<_>>());
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
        let old_name = read_let_name(&from_stage, let_node).map_err(StoreError::TransformError)?;
        let new_stage = lex_ast::rename_local(&from_stage, let_node, new_name)
            .map_err(StoreError::TransformError)?;
        let sig = lex_ast::sig_id(&from_stage).ok_or(StoreError::CannotPublishImport)?;
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
        let op = lex_vcs::Operation::new(kind, head_now.into_iter().collect::<Vec<_>>());
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
        let binding_name =
            read_let_name(&from_stage, let_node).map_err(StoreError::TransformError)?;
        let new_stage =
            lex_ast::inline_let(&from_stage, let_node).map_err(StoreError::TransformError)?;
        let sig = lex_ast::sig_id(&from_stage).ok_or(StoreError::CannotPublishImport)?;
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
        let op = lex_vcs::Operation::new(kind, head_now.into_iter().collect::<Vec<_>>());
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

        let source_sig = lex_ast::sig_id(&from_stage).ok_or(StoreError::CannotPublishImport)?;
        let new_fn_sig = lex_ast::sig_id(&new_fn_stage).ok_or(StoreError::CannotPublishImport)?;
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
            lex_ast::Stage::FnDecl(fd) => fd.effects.iter().map(|e| e.name.clone()).collect(),
            _ => Default::default(),
        };
        let new_fn_budget = budget_of_stage(&new_fn_stage);
        let mut candidate_with_new_fn: Vec<lex_ast::Stage> = Vec::with_capacity(head.len() + 1);
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
                // Single-op apply path — no package context here.
                in_file: None,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        )
        .with_intent(intent_id.clone());
        let add_transition = lex_vcs::StageTransition::Create {
            sig_id: new_fn_sig.clone(),
            stage_id: new_fn_stage_id.clone(),
        };
        let add_op_id =
            self.apply_operation_checked(branch, add_op, add_transition, &candidate_with_new_fn)?;

        // Step 2 — emit the ModifyBody op for the source. Build
        // the candidate program by replacing the source's stage
        // with `modified_stage` and keeping the new fn alongside.
        let from_budget = budget_of_stage(&from_stage);
        let to_budget = budget_of_stage(&modified_stage);
        let mut candidate_with_modified: Vec<lex_ast::Stage> = Vec::with_capacity(head.len() + 1);
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
        )
        .with_intent(intent_id);
        let modify_transition = lex_vcs::StageTransition::Replace {
            sig_id: source_sig,
            from: from_stage_id.to_string(),
            to: modified_stage_id,
        };
        let modify_op_id = self.apply_operation_checked(
            branch,
            modify_op,
            modify_transition,
            &candidate_with_modified,
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
        let sig = lex_ast::sig_id(new_stage).ok_or(StoreError::CannotPublishImport)?;
        let stage_id = self.publish(new_stage)?;
        let head_now = self.get_branch(branch)?.and_then(|b| b.head_op);
        let op = lex_vcs::Operation::new(
            lex_vcs::OperationKind::Candidate {
                sig_id: sig,
                stage_id,
            },
            head_now.into_iter().collect::<Vec<_>>(),
        )
        .with_intent(intent_id.clone());
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
            if let lex_vcs::OperationKind::Promote {
                sig_id: s,
                winner_candidate,
                supersedes,
                ..
            } = &rec.op.kind
            {
                if s != sig_id {
                    continue;
                }
                referenced.insert(winner_candidate.clone());
                for sup in supersedes {
                    referenced.insert(sup.clone());
                }
            }
        }
        let mut out: Vec<CandidateInfo> = Vec::new();
        for rec in all {
            let lex_vcs::OperationKind::Candidate {
                sig_id: s,
                stage_id,
            } = &rec.op.kind
            else {
                continue;
            };
            if s != sig_id {
                continue;
            }
            if referenced.contains(&rec.op_id) {
                continue;
            }
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
        let candidate_rec = log
            .get(candidate_op_id)?
            .ok_or_else(|| StoreError::UnknownOp(candidate_op_id.clone()))?;
        let (sig, winner_stage_id) = match &candidate_rec.op.kind {
            lex_vcs::OperationKind::Candidate { sig_id, stage_id } => {
                (sig_id.clone(), stage_id.clone())
            }
            other => {
                return Err(StoreError::InvalidTransition(format!(
                    "op `{candidate_op_id}` is a `{:?}`, not a Candidate",
                    other
                )))
            }
        };

        // #836 G4: a candidate carrying a standing `Reject` review must
        // not be promoted. "Standing" = the latest `Review` on the
        // winner's stage is a Reject; a later `Approve` (or
        // `RequestChanges`, which is advisory, not a veto) lifts it.
        // Safe by default: a candidate with no review, or an approved
        // one, promotes exactly as before.
        if let Some(lex_vcs::ReviewVerdict::Reject) = self.latest_review_verdict(&winner_stage_id)? {
            return Err(StoreError::InvalidTransition(format!(
                "candidate `{candidate_op_id}` has a standing Reject review on stage                  `{winner_stage_id}`; record an Approve review (or promote a different                  candidate) before promoting"
            )));
        }

        // Gather every OTHER live candidate for this sig — the
        // ones this Promote will supersede.
        let live = self.list_candidates(&sig)?;
        let mut supersedes: Vec<lex_vcs::OpId> = live
            .iter()
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
        let from_budget = from_stage_id
            .as_deref()
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
            self.run_required_attestations_gate(branch, &new_head.op_id, &attestable, &op_effects)
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
            let parent = self.get_branch(branch)?.and_then(|b| b.head_op);

            // Rebuild the op against the current head, but only
            // on retries (not the caller's first attempt) and
            // only for single-parent operations. Multi-parent
            // (merge) ops are passed through unchanged.
            //
            // Empty-parents ops also rebuild on attempt 1 (see
            // the exception note above) so concurrent apply
            // doesn't false-positive on StaleParent.
            let should_rebuild = is_rebuildable
                && (rebuilt_already || (current_op.parents.is_empty() && parent.is_some()));
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
        let new_op_candidate: Vec<(
            lex_vcs::OpId,
            Option<String>,
            std::collections::BTreeSet<String>,
        )> = if stage_ids.is_empty() {
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
        let waivers =
            crate::policy::check_required_attestations(&attest_log, &new_op_candidate, &policy)
                .map_err(StoreError::BranchAdvanceBlocked)?;
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
    fn collect_ancestor_candidates(&self, branch: &str) -> Result<Vec<GateCandidate>, StoreError> {
        let b = match self.get_branch(branch)? {
            Some(b) => b,
            None => return Ok(Vec::new()),
        };
        let Some(head) = b.head_op else {
            return Ok(Vec::new());
        };
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
        AddFunction { sig_id, .. }
        | ModifyBody { sig_id, .. }
        | ChangeEffectSig { sig_id, .. }
        | AddType { sig_id, .. }
        | ModifyType { sig_id, .. } => Some(sig_id.clone()),
        RenameSymbol { to, .. } => Some(to.clone()),
        _ => None,
    };
    let target_sig = target_sig?;
    stages
        .iter()
        .find(|s| sig_id(s).as_deref() == Some(target_sig.as_str()))
}

fn transition_for_kind(kind: &lex_vcs::OperationKind) -> lex_vcs::StageTransition {
    use lex_vcs::OperationKind::*;
    use lex_vcs::StageTransition;
    match kind {
        AddFunction {
            sig_id, stage_id, ..
        }
        | AddType { sig_id, stage_id, .. } => StageTransition::Create {
            sig_id: sig_id.clone(),
            stage_id: stage_id.clone(),
        },
        RemoveFunction {
            sig_id,
            last_stage_id,
        }
        | RemoveType {
            sig_id,
            last_stage_id,
        } => StageTransition::Remove {
            sig_id: sig_id.clone(),
            last: last_stage_id.clone(),
        },
        ModifyBody {
            sig_id,
            from_stage_id,
            to_stage_id,
            ..
        }
        | ChangeEffectSig {
            sig_id,
            from_stage_id,
            to_stage_id,
            ..
        }
        | ModifyType {
            sig_id,
            from_stage_id,
            to_stage_id,
        }
        | ReplaceMatchArm {
            sig_id,
            from_stage_id,
            to_stage_id,
            ..
        }
        | RenameLocal {
            sig_id,
            from_stage_id,
            to_stage_id,
            ..
        }
        | InlineLet {
            sig_id,
            from_stage_id,
            to_stage_id,
            ..
        } => StageTransition::Replace {
            sig_id: sig_id.clone(),
            from: from_stage_id.clone(),
            to: to_stage_id.clone(),
        },
        RenameSymbol {
            from,
            to,
            body_stage_id,
        } => StageTransition::Rename {
            from: from.clone(),
            to: to.clone(),
            body_stage_id: body_stage_id.clone(),
        },
        AddImport { .. } | RemoveImport { .. } => StageTransition::ImportOnly,
        Merge { .. } => StageTransition::Merge {
            entries: Default::default(),
        },
        // #294: a Candidate proposes a stage without advancing
        // the branch. ImportOnly keeps the branch head untouched
        // — the stage IS published on disk (Store::propose_candidate
        // calls publish before apply), but no head delta lands.
        Candidate { .. } => StageTransition::ImportOnly,
        // A Promote advances the head exactly like ModifyBody
        // (or Create when the sig had no head). The winner
        // stage is the new branch state for that sig.
        Promote {
            sig_id,
            winner_stage_id,
            from_stage_id,
            ..
        } => match from_stage_id {
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

/// Producer for attestations the hosted CI runner writes (#93). A
/// distinct tool name so a `require-attestation` gate — via the
/// producer-trust model — can weight "the hub verified this
/// server-side" above a client-attached `TypeCheck`.
fn hub_ci_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-hub-ci".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// Verdict of a hosted-CI run over a branch head (#93).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HubCiVerdict {
    pub passed: bool,
    pub checked_stages: usize,
    pub attested_stages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Producer for the replay-comparison attestation (#836 G3). Distinct
/// tool name so the comparison lex performed is attributable
/// separately from the (external) regeneration.
fn replay_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-store-replay".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// Human/audit label for a recorded model: `provider/name` (`@version`
/// when pinned).
fn model_label(m: &lex_vcs::ModelDescriptor) -> String {
    match &m.version {
        Some(v) => format!("{}/{}@{}", m.provider, m.name, v),
        None => format!("{}/{}", m.provider, m.name),
    }
}

/// The `(sig_id, stage_id)` an op recorded producing, or `None` for a
/// transition that produces no stage (removal / import / merge) — those
/// have nothing to regenerate for a replay.
fn produced_sig_stage(t: &lex_vcs::StageTransition) -> Option<(String, String)> {
    use lex_vcs::StageTransition::*;
    match t {
        Create { sig_id, stage_id } => Some((sig_id.clone(), stage_id.clone())),
        Replace { sig_id, to, .. } => Some((sig_id.clone(), to.clone())),
        Rename { to, body_stage_id, .. } => Some((to.clone(), body_stage_id.clone())),
        Remove { .. } | ImportOnly | Merge { .. } => None,
    }
}

/// Producer identity for `Examples::Passed` attestations emitted by
/// [`Store::record_examples_passed`] (#835). Distinct tool name so
/// the activity feed can tell an auto-emitted publish-time examples
/// verdict apart from an `lex agent-tool --examples` one.
fn examples_producer() -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: "lex-store::examples".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        model: None,
    }
}

/// Producer identity for `Review` attestations (#836). The reviewer's
/// own id lives in the kind; this records which tool minted the record.
fn review_producer(reviewer: &str) -> lex_vcs::ProducerDescriptor {
    lex_vcs::ProducerDescriptor {
        tool: format!("lex-store::review:{reviewer}"),
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
type GateCandidate = (
    lex_vcs::OpId,
    Option<String>,
    std::collections::BTreeSet<String>,
);

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
        Merge { entries } => entries.values().filter_map(|opt| opt.clone()).collect(),
        Remove { .. } | ImportOnly => Vec::new(),
    }
}

/// True when two `FnDecl`s are identical except for their body — the
/// precondition for a pure intra-body three-way merge (#838). The
/// signature fields are equal by construction when both share a
/// `sig_id`; this also guards the non-signature fields (`type_params`,
/// `examples`) so a side that changed those isn't silently dropped.
fn fndecl_same_except_body(a: &lex_ast::FnDecl, b: &lex_ast::FnDecl) -> bool {
    a.name == b.name
        && a.type_params == b.type_params
        && a.params == b.params
        && a.effects == b.effects
        && a.effect_row_var == b.effect_row_var
        && a.return_type == b.return_type
        && a.examples == b.examples
}

fn write_canonical_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let v = serde_json::to_value(value)?;
    let s = lex_ast::canon_json::to_canonical_string(&v);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
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
        return Err(lex_ast::TransformError::NonFnTarget {
            stage_kind: "non-FnDecl",
        });
    };
    // Walk back to the probed let to read its old name from the
    // *original* stage — the probed stage's let has already been
    // renamed.
    let Stage::FnDecl(orig_fd) = stage else {
        return Err(lex_ast::TransformError::NonFnTarget {
            stage_kind: "non-FnDecl",
        });
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
    let s = id
        .strip_prefix("n_")
        .ok_or_else(|| lex_ast::TransformError::BadNodeId(id.into()))?;
    let mut parts = s.split('.');
    let head = parts
        .next()
        .ok_or_else(|| lex_ast::TransformError::BadNodeId(id.into()))?;
    if head != "0" {
        return Err(lex_ast::TransformError::BadNodeId(id.into()));
    }
    let mut out = Vec::new();
    for p in parts {
        out.push(
            p.parse::<usize>()
                .map_err(|_| lex_ast::TransformError::BadNodeId(id.into()))?,
        );
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
                if idx == 0 {
                    callee
                } else {
                    args.get(idx - 1)
                        .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?
                }
            }
            Let { value, body, .. } => match idx {
                0 => value,
                1 => body,
                _ => return Err(lex_ast::TransformError::UnknownNode { at: at.into() }),
            },
            Match { scrutinee, arms } => {
                if idx == 0 {
                    scrutinee
                } else {
                    let arm_off = idx - 1;
                    if arm_off % 2 != 1 {
                        return Err(lex_ast::TransformError::UnknownNode { at: at.into() });
                    }
                    let arm_index = arm_off / 2;
                    &arms
                        .get(arm_index)
                        .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?
                        .body
                }
            }
            Block { statements, result } => {
                if idx < statements.len() {
                    &statements[idx]
                } else if idx == statements.len() {
                    result
                } else {
                    return Err(lex_ast::TransformError::UnknownNode { at: at.into() });
                }
            }
            Constructor { args, .. }
            | TupleLit { items: args, .. }
            | ListLit { items: args, .. } => args
                .get(idx)
                .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?,
            RecordLit { fields } => {
                &fields
                    .get(idx)
                    .ok_or_else(|| lex_ast::TransformError::UnknownNode { at: at.into() })?
                    .value
            }
            FieldAccess { value, .. } if idx == 0 => value,
            Lambda { body, .. } if idx == 0 => body,
            BinOp { lhs, rhs, .. } => match idx {
                0 => lhs,
                1 => rhs,
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
        Literal { .. } => "Literal",
        Var { .. } => "Var",
        Call { .. } => "Call",
        Let { .. } => "Let",
        Match { .. } => "Match",
        Block { .. } => "Block",
        Constructor { .. } => "Constructor",
        RecordLit { .. } => "RecordLit",
        TupleLit { .. } => "TupleLit",
        ListLit { .. } => "ListLit",
        FieldAccess { .. } => "FieldAccess",
        Lambda { .. } => "Lambda",
        BinOp { .. } => "BinOp",
        UnaryOp { .. } => "UnaryOp",
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
        if eff.name != "budget" {
            continue;
        }
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
