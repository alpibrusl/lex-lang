//! Files beside the op-log (#1007).
//!
//! The op-log records a package's `src/**/*.lex` semantically; everything
//! else a repository holds — `README.md`, `lex.toml`, `lex.lock`, `tests/`,
//! CI config, images — lives here as content-addressed blobs named by a
//! [`Manifest`]: a full `path → blob` snapshot, like a git tree.
//!
//! The manifest is itself a blob. Its id is the SHA-256 of its **canonical
//! JSON**: compact (`serde_json::to_vec`), struct fields in declaration order,
//! entries in a `BTreeMap` so paths are sorted —
//!
//! ```text
//! {"version":1,"entries":{"README.md":{"blob":"<sha>","mode":"100644","size":1234}}}
//! ```
//!
//! [`Manifest::from_bytes`] accepts only that exact encoding, so one file set
//! has exactly one manifest id.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use lex_vcs::{OpId, OpLog, OperationKind, OperationRecord};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// Content hash of a blob: lowercase hex SHA-256 of its exact bytes.
pub use lex_vcs::BlobId;

/// The only manifest format version this build reads and writes.
pub const MANIFEST_VERSION: u32 = 1;

/// A regular file.
pub const MODE_FILE: &str = "100644";
/// An executable file.
pub const MODE_EXEC: &str = "100755";

/// Whether `s` has the shape of a [`BlobId`] (64 lowercase hex chars).
pub fn is_blob_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One file in a [`Manifest`]. Field order is part of the canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub blob: BlobId,
    /// `100644` or `100755`; symlinks and submodules are not supported.
    pub mode: String,
    /// Byte length of the blob, so a client can chunk a fetch before
    /// downloading anything.
    pub size: u64,
}

/// A full snapshot of the repository's non-op-log files. Field order is part
/// of the canonical form — do not reorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub entries: BTreeMap<String, Entry>,
}

/// Why a manifest (or one of its paths) is not acceptable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest is not valid JSON of the expected shape: {0}")]
    Malformed(String),
    #[error("manifest is not in canonical form")]
    NotCanonical,
    #[error("unsupported manifest version {0}")]
    UnsupportedVersion(u32),
    #[error("invalid path `{path}`: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("path `{0}` is owned by the op-log (src/**/*.lex, src.lex)")]
    ReservedPath(String),
    #[error("paths `{0}` and `{1}` differ only in case")]
    CaseCollision(String, String),
    #[error("`{0}` is a file but `{1}` needs it to be a directory")]
    FileDirCollision(String, String),
    #[error("`{path}`: unsupported mode `{mode}` (only 100644 and 100755)")]
    InvalidMode { path: String, mode: String },
    #[error("`{path}`: `{blob}` is not a blob id")]
    InvalidBlobId { path: String, blob: String },
    #[error("`{path}`: manifest says {expected} byte(s), blob holds {actual}")]
    SizeMismatch {
        path: String,
        expected: u64,
        actual: u64,
    },
}

/// Whether `path` belongs to the op-log rather than the manifest:
/// `src.lex`, or any `*.lex` file under `src/`.
pub fn is_reserved_path(path: &str) -> bool {
    path == "src.lex" || (path.starts_with("src/") && path.ends_with(".lex"))
}

/// Validate one manifest path in isolation: relative, `/`-separated, no
/// empty/`.`/`..` components, no `\` or NUL, no `.git` component (any case),
/// no top-level `.lex` (the local store), and not op-log-owned.
pub fn validate_path(path: &str) -> Result<(), ManifestError> {
    let bad = |reason| {
        Err(ManifestError::InvalidPath {
            path: path.to_string(),
            reason,
        })
    };
    if path.is_empty() {
        return bad("empty");
    }
    if path.starts_with('/') {
        return bad("absolute");
    }
    if path.contains('\\') {
        return bad("contains `\\`");
    }
    if path.contains('\0') {
        return bad("contains NUL");
    }
    for (i, comp) in path.split('/').enumerate() {
        match comp {
            "" => return bad("empty component"),
            "." | ".." => return bad("`.` or `..` component"),
            c if c.eq_ignore_ascii_case(".git") => return bad("`.git` component"),
            c if i == 0 && c.eq_ignore_ascii_case(".lex") => return bad("`.lex` store directory"),
            _ => {}
        }
    }
    if is_reserved_path(path) {
        return Err(ManifestError::ReservedPath(path.to_string()));
    }
    Ok(())
}

impl Manifest {
    pub fn new() -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            entries: BTreeMap::new(),
        }
    }

    /// Check every path, mode and blob id, plus the cross-entry rules
    /// (case-only collisions, a path used as both file and directory).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.version != MANIFEST_VERSION {
            return Err(ManifestError::UnsupportedVersion(self.version));
        }
        let mut folded: BTreeMap<String, &str> = BTreeMap::new();
        for (path, e) in &self.entries {
            validate_path(path)?;
            if e.mode != MODE_FILE && e.mode != MODE_EXEC {
                return Err(ManifestError::InvalidMode {
                    path: path.clone(),
                    mode: e.mode.clone(),
                });
            }
            if !is_blob_id(&e.blob) {
                return Err(ManifestError::InvalidBlobId {
                    path: path.clone(),
                    blob: e.blob.clone(),
                });
            }
            if let Some(prev) = folded.insert(path.to_lowercase(), path) {
                return Err(ManifestError::CaseCollision(prev.to_string(), path.clone()));
            }
        }
        // A file `a` and a file `a/b` cannot both be materialized. Every
        // proper prefix directory of every path must not itself be a file.
        let files: BTreeSet<&str> = self.entries.keys().map(String::as_str).collect();
        for path in &files {
            let mut end = 0;
            while let Some(i) = path[end..].find('/') {
                end += i;
                let dir = &path[..end];
                if files.contains(dir) {
                    return Err(ManifestError::FileDirCollision(
                        dir.to_string(),
                        path.to_string(),
                    ));
                }
                end += 1;
            }
        }
        Ok(())
    }

    /// The canonical encoding — the bytes whose SHA-256 is the manifest id.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("manifest serialization is infallible")
    }

    /// The manifest's id: SHA-256 of [`Self::to_canonical_bytes`].
    pub fn id(&self) -> BlobId {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(self.to_canonical_bytes()))
    }

    /// Decode and validate a manifest. Rejects anything that is not already
    /// in canonical form, so a manifest blob's id is always [`Self::id`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ManifestError> {
        let m: Manifest =
            serde_json::from_slice(bytes).map_err(|e| ManifestError::Malformed(e.to_string()))?;
        if m.to_canonical_bytes() != bytes {
            return Err(ManifestError::NotCanonical);
        }
        m.validate()?;
        Ok(m)
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// Validate `manifest` and store it as a blob. Returns its id.
    pub fn put_manifest(&self, manifest: &Manifest) -> Result<BlobId, StoreError> {
        manifest.validate().map_err(StoreError::InvalidManifest)?;
        self.put_blob_bytes(&manifest.to_canonical_bytes())
    }

    /// Load and validate the manifest stored under `id`.
    pub fn get_manifest(&self, id: &str) -> Result<Manifest, StoreError> {
        Manifest::from_bytes(&self.get_blob_bytes(id)?).map_err(StoreError::InvalidManifest)
    }

    /// The entry blobs of `manifest` this store does not hold, sorted and
    /// deduplicated. Empty means the manifest's closure is complete.
    pub fn manifest_closure_missing(&self, manifest: &Manifest) -> Vec<BlobId> {
        let ids: BTreeSet<&BlobId> = manifest.entries.values().map(|e| &e.blob).collect();
        ids.into_iter()
            .filter(|id| !self.has_blob(id))
            .cloned()
            .collect()
    }
}

// ── SetFiles: the manifest in the op DAG (#1007 PR 2) ───────────────────────

/// Why an op has nothing to replay, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum NotReplayable {
    /// A `SetFiles` snapshot: files are recorded, not regenerated.
    #[error("files snapshot (set_files) — not a program change")]
    Files,
}

/// Refuse a non-semantic op before any replay machinery sees it.
pub(crate) fn refuse_non_semantic(rec: &OperationRecord) -> Result<(), StoreError> {
    match rec.op.kind {
        OperationKind::SetFiles { .. } => Err(StoreError::NotReplayable {
            op_id: rec.op_id.clone(),
            why: NotReplayable::Files,
        }),
        _ => Ok(()),
    }
}

/// The files manifest in force at an op.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ManifestAt {
    /// No `SetFiles` in the op's history: the repository has no files
    /// beyond the op-log.
    Absent,
    /// The manifest set by the nearest `SetFiles` ancestor (or the op
    /// itself).
    Set { manifest: BlobId },
    /// A merge whose parents carry different manifests (or an ancestor
    /// that is itself ambiguous). There is no well-defined file set until
    /// a `SetFiles` on top of the merge records the merged manifest. Fails
    /// closed; independent of parent order.
    Ambiguous,
}

impl ManifestAt {
    /// The manifest id, when there is exactly one.
    pub fn manifest(&self) -> Option<&BlobId> {
        match self {
            ManifestAt::Set { manifest } => Some(manifest),
            _ => None,
        }
    }
}

/// Combine parents' manifests: all equal → that value; otherwise
/// `Ambiguous`. No parents → `Absent`. Symmetric, so parent order (which
/// `Operation::new` sorts by OpId anyway) cannot change the answer.
fn combine<'a>(mut parents: impl Iterator<Item = &'a ManifestAt>) -> ManifestAt {
    let Some(first) = parents.next() else {
        return ManifestAt::Absent;
    };
    if parents.all(|p| p == first) {
        first.clone()
    } else {
        ManifestAt::Ambiguous
    }
}

/// `manifest_at(op)` over the DAG, iteratively (histories are deep).
///
/// `memo` carries known answers in and out — a head snapshot seeds it with
/// the snapshot op's value, so extending a cached head only visits new ops.
/// `pre` holds records already in memory (e.g. from `walk_forward_since`),
/// consulted before the on-disk log. A walk stops at the nearest `SetFiles`
/// on every path. An op missing from an incomplete local log counts as
/// `Absent`, the same leniency `branch_head` applies to its transitions.
pub(crate) fn resolve_manifest_at(
    log: &OpLog,
    op: &OpId,
    memo: &mut HashMap<OpId, ManifestAt>,
    pre: &HashMap<&str, &OperationRecord>,
) -> Result<ManifestAt, StoreError> {
    let mut parents_of: HashMap<OpId, Vec<OpId>> = HashMap::new();
    let mut stack: Vec<OpId> = vec![op.clone()];
    while let Some(id) = stack.last().cloned() {
        if memo.contains_key(&id) {
            stack.pop();
            continue;
        }
        if !parents_of.contains_key(&id) {
            let loaded;
            let rec = match pre.get(id.as_str()) {
                Some(r) => Some(*r),
                None => {
                    loaded = log.get(&id)?;
                    loaded.as_ref()
                }
            };
            match rec {
                None => {
                    memo.insert(id, ManifestAt::Absent);
                    stack.pop();
                    continue;
                }
                Some(r) => {
                    if let OperationKind::SetFiles { manifest } = &r.op.kind {
                        memo.insert(
                            id,
                            ManifestAt::Set {
                                manifest: manifest.clone(),
                            },
                        );
                        stack.pop();
                        continue;
                    }
                    parents_of.insert(id.clone(), r.op.parents.clone());
                }
            }
        }
        let parents = &parents_of[&id];
        let pending: Vec<OpId> = parents
            .iter()
            .filter(|p| !memo.contains_key(*p))
            .cloned()
            .collect();
        if !pending.is_empty() {
            stack.extend(pending);
            continue;
        }
        let value = combine(parents.iter().map(|p| &memo[p]));
        memo.insert(id, value);
        stack.pop();
    }
    Ok(memo[op].clone())
}

impl Store {
    /// The files manifest in force at `op_id` (#1007): its own if it is a
    /// `SetFiles`, else inherited through its parents — see [`ManifestAt`].
    /// `UnknownOp` if `op_id` is not in the log.
    pub fn manifest_at(&self, op_id: &str) -> Result<ManifestAt, StoreError> {
        let log = OpLog::open(self.root())?;
        let op_id = op_id.to_string();
        if log.get(&op_id)?.is_none() {
            return Err(StoreError::UnknownOp(op_id));
        }
        resolve_manifest_at(&log, &op_id, &mut HashMap::new(), &HashMap::new())
    }

    /// Check that `manifest` may become the file set of a head: it decodes
    /// as a canonical, valid manifest (no reserved `src/**/*.lex` paths),
    /// every entry blob is present, and each blob's length matches its
    /// entry's `size`. Writes nothing.
    pub fn validate_set_files(&self, manifest: &str) -> Result<Manifest, StoreError> {
        if !self.has_blob(manifest) {
            return Err(StoreError::MissingBlobs(vec![manifest.to_string()]));
        }
        let m = self.get_manifest(manifest)?;
        let missing = self.manifest_closure_missing(&m);
        if !missing.is_empty() {
            return Err(StoreError::MissingBlobs(missing));
        }
        for (path, e) in &m.entries {
            let actual = self.blob_len(&e.blob).unwrap_or(0);
            if actual != e.size {
                return Err(StoreError::InvalidManifest(ManifestError::SizeMismatch {
                    path: path.clone(),
                    expected: e.size,
                    actual,
                }));
            }
        }
        Ok(m)
    }

    /// Append a `SetFiles { manifest }` op to `branch` (#1007): the
    /// repository's non-op-log files become exactly the snapshot `manifest`
    /// names. The sig→stage map is untouched.
    ///
    /// Validated first ([`Self::validate_set_files`]); on any failure
    /// nothing is persisted and the head is unchanged. Goes through the
    /// same CAS advance (and branch-advance policy gate) as every other op.
    /// Always appends: a caller that wants "no op when unchanged" compares
    /// against [`Self::branch_manifest`] first.
    pub fn apply_set_files(
        &self,
        branch: &str,
        manifest: &str,
        intent_id: Option<&lex_vcs::IntentId>,
    ) -> Result<OpId, StoreError> {
        self.validate_set_files(manifest)?;
        let head = self.get_branch(branch)?.and_then(|b| b.head_op);
        let mut op = lex_vcs::Operation::new(
            OperationKind::SetFiles {
                manifest: manifest.to_string(),
            },
            head,
        );
        if let Some(i) = intent_id {
            op = op.with_intent(i.clone());
        }
        self.apply_operation(branch, op, lex_vcs::StageTransition::FilesOnly)
    }
}
