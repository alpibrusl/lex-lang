//! `<store>/policy.json` — local trust policy (#181, originally
//! called out in the v3 acceptance criteria for #172).
//!
//! v3 introduced human-issued attestations (`Override`, `Defer`,
//! `Block`, `Unblock`); v3 follow-up adds the inverse: the human
//! can signal that *agent*-issued attestations from a specific
//! producer shouldn't be trusted. Enforcement is at attestation-
//! read time — the on-disk attestation log keeps the original
//! record, and consumers (web activity feed, CI gates, etc.)
//! consult the policy to decide whether to surface a `blocked`
//! tag or filter the row out.
//!
//! File schema (deliberately small, mirroring `users.json`):
//!
//! ```json
//! {
//!   "blocked_producers": [
//!     {"tool": "buggy-bot", "reason": "false positives", "blocked_at": 1714960000}
//!   ]
//! }
//! ```
//!
//! Matching is against `Attestation::produced_by.tool`. `model`
//! is intentionally not part of the match key in v1 — the tool
//! identifier is stable across model upgrades. Add a `model`
//! field if a future use case actually needs it.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedProducer {
    /// Matched against `ProducerDescriptor::tool`.
    pub tool: String,
    pub reason: String,
    /// Wall-clock seconds since epoch when the block was added.
    /// Useful for "blocked since X" rendering in the activity feed.
    pub blocked_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyFile {
    #[serde(default)]
    pub blocked_producers: Vec<BlockedProducer>,
}

/// Load `<root>/policy.json`. Returns `Ok(None)` when absent
/// (no policy → no blocks); `Ok(Some(default))` when the file
/// exists but is empty/has no blocks.
pub fn load(root: &Path) -> io::Result<Option<PolicyFile>> {
    let path = root.join("policy.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let file: PolicyFile = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData,
            format!("parsing {}: {e}", path.display())))?;
    Ok(Some(file))
}

/// Atomic write: tempfile + rename so a crashed write never
/// leaves a half-truncated `policy.json`. Same pattern the
/// attestation log uses.
pub fn save(root: &Path, file: &PolicyFile) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let path = root.join("policy.json");
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(file)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)
}

impl PolicyFile {
    /// Whether the named tool is on the block list.
    pub fn is_blocked(&self, tool: &str) -> bool {
        self.blocked_producers.iter().any(|p| p.tool == tool)
    }

    /// Look up the block entry, if any. Useful for "blocked
    /// since X — reason: Y" rendering.
    pub fn find(&self, tool: &str) -> Option<&BlockedProducer> {
        self.blocked_producers.iter().find(|p| p.tool == tool)
    }

    /// Add a producer to the block list. Idempotent: blocking an
    /// already-blocked tool is a no-op (preserves the original
    /// `blocked_at`); the new reason is dropped. Callers that
    /// want to update a reason should `unblock` then `block`.
    pub fn block(&mut self, tool: String, reason: String, now: u64) {
        if self.is_blocked(&tool) {
            return;
        }
        self.blocked_producers.push(BlockedProducer {
            tool,
            reason,
            blocked_at: now,
        });
    }

    /// Remove a producer from the block list. Returns whether
    /// the entry was present.
    pub fn unblock(&mut self, tool: &str) -> bool {
        let before = self.blocked_producers.len();
        self.blocked_producers.retain(|p| p.tool != tool);
        before != self.blocked_producers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_absent_returns_none() {
        let tmp = tempdir().unwrap();
        assert!(load(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn round_trip_through_disk() {
        let tmp = tempdir().unwrap();
        let mut f = PolicyFile::default();
        f.block("bot-a".into(), "false positives".into(), 1000);
        f.block("bot-b".into(), "stale model".into(), 2000);
        save(tmp.path(), &f).unwrap();
        let got = load(tmp.path()).unwrap().unwrap();
        assert_eq!(got, f);
        assert!(got.is_blocked("bot-a"));
        assert!(!got.is_blocked("not-blocked"));
        assert_eq!(got.find("bot-b").unwrap().reason, "stale model");
    }

    #[test]
    fn block_is_idempotent() {
        let mut f = PolicyFile::default();
        f.block("bot".into(), "first reason".into(), 100);
        f.block("bot".into(), "second reason — ignored".into(), 200);
        assert_eq!(f.blocked_producers.len(), 1);
        // Original blocked_at + reason preserved.
        let entry = f.find("bot").unwrap();
        assert_eq!(entry.blocked_at, 100);
        assert_eq!(entry.reason, "first reason");
    }

    #[test]
    fn unblock_removes_entry() {
        let mut f = PolicyFile::default();
        f.block("bot".into(), "x".into(), 1);
        assert!(f.unblock("bot"));
        assert!(!f.is_blocked("bot"));
        // Second unblock is a no-op and returns false.
        assert!(!f.unblock("bot"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("policy.json"), "{ not json").unwrap();
        let err = load(tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
