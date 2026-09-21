//! Persisted store records.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Draft,
    Active,
    Deprecated,
    Tombstone,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transition {
    pub stage_id: String,
    pub from: StageStatus,
    pub to: StageStatus,
    pub at: u64,            // unix seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Per-`SigId` lifecycle log: append-only list of state transitions for
/// every implementation that's ever been published under this signature.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Lifecycle {
    pub sig_id: String,
    pub transitions: Vec<Transition>,
}

impl Lifecycle {
    /// Current status of a given implementation.
    pub fn status_of(&self, stage_id: &str) -> Option<StageStatus> {
        self.transitions
            .iter()
            .rev()
            .find(|t| t.stage_id == stage_id)
            .map(|t| t.to)
    }

    /// The currently-Active StageId for this signature, if any.
    pub fn current_active(&self) -> Option<&str> {
        // Walk transitions chronologically; track latest status per stage.
        use indexmap::IndexMap;
        let mut latest: IndexMap<&str, StageStatus> = IndexMap::new();
        for t in &self.transitions {
            latest.insert(&t.stage_id, t.to);
        }
        latest
            .into_iter()
            .find(|(_, s)| *s == StageStatus::Active)
            .map(|(id, _)| id)
    }
}

/// Per-implementation metadata (`<StageId>.metadata.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Metadata {
    pub stage_id: String,
    pub sig_id: String,
    /// Human-friendly name (e.g. "factorial"). Lives here, not in the
    /// implementation hash, so renames don't change StageId.
    pub name: String,
    pub published_at: u64,
    /// Free-form notes (e.g. "fixes overflow on n>20").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The declaration's own `#` comments, in source order, each including
    /// its leading `#`.
    ///
    /// `lex-syntax` parses and attaches comments per declaration, and the
    /// canonicalizer strips them so they never participate in a SigId or
    /// StageId. That exclusion is right and load-bearing: editing a comment
    /// must not read as a code change, or exact replay would demand a model
    /// reproduce prose verbatim.
    ///
    /// But the op-log stored only the canonical (stripped) AST, and a registry
    /// archive is *rendered* from that log — so every hosted package was served
    /// with all documentation removed. `lex-ocpi` lost 2051 comment lines,
    /// `lex-agent` 519; the module header explaining a package's schema and
    /// purpose simply vanished between GitHub and the registry.
    ///
    /// They live here for exactly the reason `name` does: outside the hash, so
    /// they survive the round trip without disturbing content addressing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub doc: Vec<String>,
    /// Optional Ed25519 signature over the UTF-8 bytes of `stage_id`
    /// (#227). Set on publish when the caller provides a signing key;
    /// consumers verify via [`lex_vcs::verify_stage_id`]. Absence
    /// means "unsigned" — policy decides whether to accept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<lex_vcs::Signature>,
}

/// A test attached to a SigId (spec §4.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Test {
    pub id: String,
    pub kind: String,
    pub input: serde_json::Value,
    pub expected_output: serde_json::Value,
    #[serde(default)]
    pub effects_allowed: Vec<String>,
}

/// A spec attached to a SigId (spec §4.4). Kept opaque here — the
/// spec-checker (M10) interprets its body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Spec {
    pub id: String,
    pub kind: String,
    pub body: serde_json::Value,
}
