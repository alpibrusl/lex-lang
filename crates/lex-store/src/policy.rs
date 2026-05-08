//! `<store>/policy.json` — local trust policy.
//!
//! Two orthogonal concerns share the same file:
//!
//! 1. **`blocked_producers`** (#181) — negative gate on
//!    attestations. Producers on this list keep their attestations
//!    in the log (audit trail intact) but consumers tag those rows
//!    `blocked`. Enforcement is at attestation-read time.
//! 2. **`required_attestations`** (#245) — positive gate on
//!    *branch advancement*. Each entry says "every op landed on
//!    this branch must carry a `Passed` attestation of kind X (or
//!    of kind X *when its effects intersect Y*) before the branch
//!    head can move past it." This is the agent-shaped equivalent
//!    of "branch protection rules" in human VCSes, grounded in the
//!    attestation graph rather than human review.
//!
//! File schema (additive across versions):
//!
//! ```json
//! {
//!   "blocked_producers": [
//!     {"tool": "buggy-bot", "reason": "false positives", "blocked_at": 1714960000}
//!   ],
//!   "required_attestations": [
//!     {"kind": "type_check", "when": {"always": null}},
//!     {"kind": "spec",       "when": {"always": null}},
//!     {"kind": "sandbox_run", "when": {"effects_intersect": ["io", "net", "fs_write"]}}
//!   ]
//! }
//! ```
//!
//! Existing `policy.json` files keep working — `required_attestations`
//! defaults to empty (no gate).

use lex_vcs::{Attestation, AttestationKind, AttestationLog, AttestationResult, OpId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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
    /// Positive gate on branch advance (#245). Empty means "no
    /// requirements" — same behavior as before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_attestations: Vec<RequiredAttestation>,
}

/// One required-attestation rule. Says: "every op advancing the
/// branch must carry a `Passed` attestation of `kind`, except
/// possibly when `when` filters it out."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredAttestation {
    pub kind: RequiredAttestationKind,
    /// Defaults to [`AttestationCondition::Always`] if absent in the
    /// JSON — the typical "this attestation is mandatory for every
    /// op" rule.
    #[serde(default)]
    pub when: AttestationCondition,
}

/// Which `AttestationKind` is required. Mirrors the variants the
/// existing producers emit (`TypeCheck` from #130, `Spec` from
/// #186, `SandboxRun` from `lex agent-tool`, etc.). Only the
/// machine-emittable variants are exposed; human-only attestations
/// like `Override` and `Block` aren't useful here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredAttestationKind {
    TypeCheck,
    Spec,
    SandboxRun,
    Examples,
    DiffBody,
    EffectAudit,
}

impl RequiredAttestationKind {
    /// Match against an actual [`AttestationKind`]. Variants with
    /// payloads (e.g. `Spec { spec_id, … }`, `SandboxRun { effects }`)
    /// match the type-tag only — any spec, any sandbox run.
    pub fn matches(&self, kind: &AttestationKind) -> bool {
        matches!(
            (self, kind),
            (Self::TypeCheck, AttestationKind::TypeCheck)
                | (Self::Spec, AttestationKind::Spec { .. })
                | (Self::SandboxRun, AttestationKind::SandboxRun { .. })
                | (Self::Examples, AttestationKind::Examples { .. })
                | (Self::DiffBody, AttestationKind::DiffBody { .. })
                | (Self::EffectAudit, AttestationKind::EffectAudit)
        )
    }

    /// CLI-friendly tag used by `lex policy require-attestation
    /// <tag>` and rendered in `lex policy list`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::TypeCheck => "type_check",
            Self::Spec => "spec",
            Self::SandboxRun => "sandbox_run",
            Self::Examples => "examples",
            Self::DiffBody => "diff_body",
            Self::EffectAudit => "effect_audit",
        }
    }

    /// Inverse of [`Self::tag`] — used when parsing CLI input.
    pub fn from_tag(s: &str) -> Option<Self> {
        match s {
            "type_check" | "TypeCheck" => Some(Self::TypeCheck),
            "spec" | "Spec" => Some(Self::Spec),
            "sandbox_run" | "SandboxRun" => Some(Self::SandboxRun),
            "examples" | "Examples" => Some(Self::Examples),
            "diff_body" | "DiffBody" => Some(Self::DiffBody),
            "effect_audit" | "EffectAudit" => Some(Self::EffectAudit),
            _ => None,
        }
    }
}

/// When a [`RequiredAttestation`] applies to a given op.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttestationCondition {
    /// The attestation is required for every op.
    #[default]
    Always,
    /// Only required when the op's declared effect set intersects
    /// any of these effect strings. Matches the same effect-name
    /// shape used in `OperationKind::AddFunction.effects` etc.
    /// Empty set means the rule is effectively disabled (useful as
    /// a temporary kill-switch without removing the entry).
    EffectsIntersect(BTreeSet<String>),
}

impl AttestationCondition {
    /// Whether the rule fires for an op whose declared effects are
    /// `op_effects`. `EffectsIntersect` with an empty set never
    /// fires.
    pub fn applies(&self, op_effects: &BTreeSet<String>) -> bool {
        match self {
            AttestationCondition::Always => true,
            AttestationCondition::EffectsIntersect(needed) => {
                !needed.is_empty() && op_effects.iter().any(|e| needed.contains(e))
            }
        }
    }
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

    /// Add a `RequiredAttestation` rule. Idempotent on `(kind, when)`
    /// — the same rule submitted twice is a single entry. Different
    /// `when` clauses for the same `kind` are distinct rules and
    /// stack (e.g. "always require Spec" plus "require SandboxRun
    /// when effects intersect [io]").
    pub fn require_attestation(
        &mut self,
        kind: RequiredAttestationKind,
        when: AttestationCondition,
    ) -> bool {
        let new = RequiredAttestation { kind, when };
        if self.required_attestations.contains(&new) {
            return false;
        }
        self.required_attestations.push(new);
        true
    }

    /// Remove every rule with the given kind. Returns how many
    /// rules were removed. Use this to drop a requirement entirely;
    /// for narrowing a rule (e.g. `Always` → `EffectsIntersect`)
    /// remove + re-add.
    pub fn unrequire_attestation(&mut self, kind: RequiredAttestationKind) -> usize {
        let before = self.required_attestations.len();
        self.required_attestations
            .retain(|r| r.kind != kind);
        before - self.required_attestations.len()
    }
}

// ---------------------------------------------------------------- gate

/// Why a branch advance was refused by the [`required_attestations`]
/// gate. Surfaced as `StoreError::BranchAdvanceBlocked` and as a
/// structured envelope on the HTTP API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchAdvanceBlocked {
    pub op_id: OpId,
    /// Stage the gate was checking. `None` for ops that don't touch
    /// a stage (imports, merges) — those are always allowed because
    /// there's nothing to attest.
    pub stage_id: Option<String>,
    /// Tags of attestation kinds that were required but missing
    /// (or present only as Failed/Inconclusive).
    pub missing: Vec<String>,
}

impl BranchAdvanceBlocked {
    /// Render as a structured JSON envelope. Used by the HTTP layer
    /// and `lex` CLI's `--output json`.
    pub fn to_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "error": "BranchAdvanceBlocked",
            "op_id": self.op_id,
            "stage_id": self.stage_id,
            "missing": self.missing,
        })
    }
}

/// Verify that the candidate ops carry every required attestation.
/// Called by [`crate::Store::apply_operation`] (and friends) between
/// op persistence and branch-head advance.
///
/// `candidate` lists the ops *being added by this advance* — for the
/// single-op apply path (today's only writer) it's a one-element
/// slice. Each op is checked against the policy by walking the
/// stage's attestation list once and matching on `op_id` and kind.
///
/// Ops without an attestable `stage_id` (imports, merges) pass the
/// gate unconditionally — there's nothing to attest. The
/// content-addressed merge resolution itself isn't where evidence
/// belongs; the constituent stages on either side are.
pub fn check_required_attestations(
    log: &AttestationLog,
    candidate: &[(OpId, Option<String>, BTreeSet<String>)],
    policy: &PolicyFile,
) -> Result<(), BranchAdvanceBlocked> {
    if policy.required_attestations.is_empty() {
        return Ok(());
    }
    for (op_id, stage_id_opt, op_effects) in candidate {
        let stage_id = match stage_id_opt {
            Some(s) => s,
            // No stage to attest — skip. The policy is per-stage;
            // an import or merge doesn't have a verdict surface.
            None => continue,
        };
        let attestations = log
            .list_for_stage(stage_id)
            .map_err(|e| BranchAdvanceBlocked {
                op_id: op_id.clone(),
                stage_id: Some(stage_id.clone()),
                missing: vec![format!("io:{e}")],
            })?;
        let mut missing: Vec<String> = Vec::new();
        for rule in &policy.required_attestations {
            if !rule.when.applies(op_effects) {
                continue;
            }
            let satisfied = attestations.iter().any(|a| {
                a.op_id.as_deref() == Some(op_id.as_str())
                    && rule.kind.matches(&a.kind)
                    && passed(&a.result)
            });
            if !satisfied {
                missing.push(rule.kind.tag().to_string());
            }
        }
        if !missing.is_empty() {
            // De-dup in case the same kind is required twice with
            // different `when` clauses; the user only needs to
            // surface it once.
            missing.sort();
            missing.dedup();
            return Err(BranchAdvanceBlocked {
                op_id: op_id.clone(),
                stage_id: Some(stage_id.clone()),
                missing,
            });
        }
    }
    Ok(())
}

fn passed(r: &AttestationResult) -> bool {
    matches!(r, AttestationResult::Passed)
}

#[allow(dead_code)]
fn _force_use(_: Attestation) {} // keep unused-import warning quiet across feature flips

// ----------------------------------------------- producer-block gate (#248)

/// Why a branch advance was refused by the
/// [`check_producer_block`] gate. Surfaced as
/// `StoreError::ProducerBlocked` and as a distinct
/// `ProducerBlocked` envelope on the HTTP API.
///
/// Distinct from [`BranchAdvanceBlocked`] (#245's positive
/// attestation gate) because the response shape is different:
/// the operator needs to know *which producer* was blocked and
/// *which attestation* tripped the gate, not just "an
/// attestation was missing."
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProducerBlocked {
    pub op_id: OpId,
    /// Stage the offending attestation was about. Always set —
    /// `ProducerBlock` only fires when a candidate op has at
    /// least one stage with attestations.
    pub stage_id: String,
    /// Tool that's been retroactively quarantined.
    pub tool_id: String,
    /// Cutoff from the most recent `AttestationKind::ProducerBlock`
    /// for this tool.
    pub blocked_at: u64,
    /// Wall-clock timestamp of the offending attestation. Always
    /// `>= blocked_at`; otherwise the gate wouldn't have fired.
    pub attestation_at: u64,
    /// `attestation_id` of the offending attestation, so the
    /// operator can drill in via `lex attest filter`.
    pub attestation_id: String,
}

impl ProducerBlocked {
    /// Render as a structured JSON envelope. Distinct `error`
    /// discriminator (`ProducerBlocked`) so the HTTP layer can
    /// route it to a different status code or recovery path
    /// from `BranchAdvanceBlocked`.
    pub fn to_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "error": "ProducerBlocked",
            "op_id": self.op_id,
            "stage_id": self.stage_id,
            "tool_id": self.tool_id,
            "blocked_at": self.blocked_at,
            "attestation_at": self.attestation_at,
            "attestation_id": self.attestation_id,
        })
    }
}

/// Verify that the candidate ops are not contaminated by
/// attestations from a retroactively quarantined producer (#248).
///
/// Algorithm:
///
/// 1. Walk the entire attestation log to collect every
///    `ProducerBlock` / `ProducerUnblock` record. Build a map
///    `tool_id → Some(blocked_at)` reflecting the latest verdict
///    per tool.
/// 2. For each candidate op, list attestations on its
///    `stage_id`. For every attestation produced by a currently-
///    blocked tool with `attestation.timestamp >= block.blocked_at`,
///    refuse the advance with the offending row's details.
///
/// Cost is `O(total attestations)` for step 1 — fine for small
/// stores; a `by-tool` index becomes worthwhile if the producer
/// list grows past dozens. Step 2 is `O(attestations on the
/// candidate stage)` per op, dominated by step 1 for the common
/// case of a single-op advance.
pub fn check_producer_block(
    log: &AttestationLog,
    candidate: &[(OpId, Option<String>, BTreeSet<String>)],
) -> Result<(), ProducerBlocked> {
    // Step 1: build the active-block map.
    let all = match log.list_all() {
        Ok(v) => v,
        // Empty log = nothing to check.
        Err(_) => return Ok(()),
    };
    use std::collections::HashMap;
    let mut active: HashMap<String, u64> = HashMap::new();
    // Process in timestamp order so the latest verdict per tool
    // wins. Ties go to ProducerUnblock (matches the spirit of
    // is_stage_blocked).
    let mut ordered: Vec<&Attestation> = all.iter().collect();
    ordered.sort_by(|a, b| {
        a.timestamp.cmp(&b.timestamp).then_with(|| {
            // Same timestamp: ProducerUnblock sorts after
            // ProducerBlock so it wins the "latest" check.
            let a_unblock = matches!(a.kind, AttestationKind::ProducerUnblock { .. });
            let b_unblock = matches!(b.kind, AttestationKind::ProducerUnblock { .. });
            a_unblock.cmp(&b_unblock)
        })
    });
    for a in ordered {
        match &a.kind {
            AttestationKind::ProducerBlock { tool_id, blocked_at, .. } => {
                active.insert(tool_id.clone(), *blocked_at);
            }
            AttestationKind::ProducerUnblock { tool_id, .. } => {
                active.remove(tool_id);
            }
            _ => {}
        }
    }
    if active.is_empty() {
        return Ok(());
    }

    // Step 2: per-op attestation walk.
    for (op_id, stage_id_opt, _) in candidate {
        let stage_id = match stage_id_opt {
            Some(s) => s,
            None => continue,
        };
        let attestations = match log.list_for_stage(stage_id) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for a in attestations {
            // Self-references (a producer's own ProducerBlock /
            // Unblock attestations are stored at stage_id ==
            // tool_id) shouldn't be flagged as contamination.
            if matches!(
                a.kind,
                AttestationKind::ProducerBlock { .. }
                    | AttestationKind::ProducerUnblock { .. }
            ) {
                continue;
            }
            if let Some(&blocked_at) = active.get(&a.produced_by.tool) {
                if a.timestamp >= blocked_at {
                    return Err(ProducerBlocked {
                        op_id: op_id.clone(),
                        stage_id: stage_id.clone(),
                        tool_id: a.produced_by.tool.clone(),
                        blocked_at,
                        attestation_at: a.timestamp,
                        attestation_id: a.attestation_id.clone(),
                    });
                }
            }
        }
    }
    Ok(())
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
