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

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Store, StoreError};

/// Content hash of a blob: lowercase hex SHA-256 of its exact bytes.
pub type BlobId = String;

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
