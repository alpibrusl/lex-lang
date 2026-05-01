//! `Store` — content-addressed code repository.
//!
//! The filesystem is the source of truth. All operations read/write JSON
//! files under `<root>/stages/<SigId>/`. There is no SQLite cache: every
//! query walks the directory and parses what's needed. `cargo test`
//! runs aren't perf-critical and the §4.6 acceptance requires the
//! rebuild-from-filesystem property anyway.

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
            let metadata = Metadata {
                stage_id: stage_id.clone(),
                sig_id: sig.clone(),
                name,
                published_at: Self::now(),
                note: None,
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
        out.sort_by(|a, b| b.last_at.cmp(&a.last_at));
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
}

fn stage_name(stage: &Stage) -> &str {
    match stage {
        Stage::FnDecl(fd) => &fd.name,
        Stage::TypeDecl(td) => &td.name,
        Stage::Import(i) => &i.alias,
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
