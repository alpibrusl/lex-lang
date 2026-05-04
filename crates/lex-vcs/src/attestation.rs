//! Persistent evidence about a stage (#132).
//!
//! [`Operation`](crate::Operation) records *what* changed.
//! [`Intent`](crate::Intent) records *why*. An [`Attestation`] records
//! *what we know about the result*: did this stage typecheck, did its
//! examples pass, did a spec prove it, did `lex agent-tool` run it
//! cleanly under a sandbox.
//!
//! Today every verification (`lex check`, `lex agent-tool --spec ...`,
//! `lex audit --effect ...`) runs, prints a verdict, and exits. The
//! evidence is ephemeral — there's no persistent answer to "has this
//! stage ever been spec-checked?" beyond rerunning. That makes
//! attestations useless as a CI gate and useless as a trust signal
//! across sessions.
//!
//! This module is the foundational data layer for tier-2's evidence
//! story. Producers (`lex check` emits `TypeCheck`, `lex agent-tool`
//! emits `Spec` / `Examples` / `DiffBody` / `SandboxRun`) and
//! consumers (`lex blame --with-evidence`, `GET /v1/stage/<id>/
//! attestations`) wire to it in subsequent slices.
//!
//! # Identity
//!
//! [`AttestationId`] is the lowercase-hex SHA-256 of the canonical
//! form of `(stage_id, op_id, intent_id, kind, result, produced_by)`.
//! `cost`, `timestamp`, and `signature` are deliberately *not* in the
//! hash so two independent runs of the same logical verification —
//! same stage, same kind, same producer, same outcome — produce the
//! same `attestation_id`. This is the dedup property the issue calls
//! out: harnesses can ask "has this exact verification been done?"
//! by checking for the id without rerunning.
//!
//! # Storage
//!
//! ```text
//! <root>/attestations/<AttestationId>.json
//! <root>/attestations/by-stage/<StageId>/<AttestationId>
//! ```
//!
//! The primary file under `attestations/` is the source of truth.
//! `by-stage/` is a per-stage index — empty marker files whose names
//! point at the primary record. Rebuildable from primary records on
//! demand; we write it eagerly so `lex stage <id> --attestations` is
//! a directory listing rather than a full scan.
//!
//! `by-spec/` (mentioned in the issue) is deferred until a producer
//! actually emits `Spec` attestations against persisted spec ids.
//!
//! # Trust model
//!
//! Attestations are claims, not proofs. The store doesn't trust
//! attestations from outside — it just stores them. A maintainer
//! choosing to skip CI for a stage that already has a passing spec
//! attestation from a known producer is a *policy* decision, not a
//! guarantee the store enforces. The optional Ed25519 signature
//! field exists so an attestation can be cryptographically tied to
//! a producer (e.g. a CI runner's public key) and the policy
//! decision auditable. Verifying signatures is out of scope for the
//! data layer.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::canonical;
use crate::intent::IntentId;
use crate::operation::{OpId, StageId};

/// Content-addressed identity of an attestation. Lowercase-hex
/// SHA-256 of the canonical form of
/// `(stage_id, op_id, intent_id, kind, result, produced_by)`.
pub type AttestationId = String;

/// Reference to a spec file. Free-form string so callers can use
/// either a content hash or a logical name; the data layer doesn't
/// care which. Producers should pick one and stick with it for
/// dedup to work as expected.
pub type SpecId = String;

/// Content hash of a file (examples list, body source, etc.). Kept
/// as a string for the same reason as [`OpId`]: we want this crate
/// to have no view into the hash function used by callers.
pub type ContentHash = String;

/// What was verified. The variants mirror the verdict surfaces
/// `lex agent-tool` and the store-write gate already produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttestationKind {
    /// `lex agent-tool --examples FILE` — body was run against
    /// `{input, expected}` pairs.
    Examples {
        file_hash: ContentHash,
        count: usize,
    },
    /// `lex spec check` or `lex agent-tool --spec FILE` — a
    /// behavioral contract was checked against the body.
    Spec {
        spec_id: SpecId,
        method: SpecMethod,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trials: Option<usize>,
    },
    /// `lex agent-tool --diff-body 'src'` — a second body was run on
    /// the same inputs and the outputs compared.
    DiffBody {
        other_body_hash: ContentHash,
        input_count: usize,
    },
    /// Emitted by the store-write gate (#130) on every accepted op.
    /// The store can answer "the HEAD typechecks" as a queryable
    /// fact rather than an implicit invariant.
    TypeCheck,
    /// Emitted by `lex audit --effect K` when no violations are
    /// found. Useful as a trust signal that a stage was checked
    /// against a specific effect-policy revision.
    EffectAudit,
    /// Emitted by `lex agent-tool` on a successful sandboxed run.
    /// `effects` is the set the sandbox actually allowed; useful for
    /// answering "did this code run under fs_write?" after the fact.
    SandboxRun {
        effects: BTreeSet<String>,
    },
}

/// Verification method for [`AttestationKind::Spec`]. Mirrors the
/// tag the spec checker already uses — kept as a string so the
/// vcs crate doesn't have to pull `spec-checker` in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecMethod {
    /// Exhaustive search; `trials` is unset.
    Exhaustive,
    /// Random sampling; `trials` carries the sample count.
    Random,
    /// Symbolic execution.
    Symbolic,
}

/// Whether the verification succeeded. `Inconclusive` is its own
/// state because some checkers (e.g. random-sampling spec checks
/// over an unbounded input space) can pass within their budget
/// without proving the contract holds in general.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum AttestationResult {
    Passed,
    Failed { detail: String },
    Inconclusive { detail: String },
}

/// Who produced this attestation. `tool` is the CLI / harness name
/// (`"lex check"`, `"lex agent-tool"`, `"ci-runner@v3"`). `version`
/// pins the tool revision so a regression in the producer is
/// distinguishable from a regression in the code being verified.
/// `model` is set when an LLM was the proximate producer — for
/// `--spec`-style runs the harness is the producer; for `lex
/// agent-tool` the model is, and we want both recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerDescriptor {
    pub tool: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Optional cost record. Excluded from the attestation hash so
/// rerunning a verification on a different machine (different
/// wall-clock, different token pricing) doesn't break dedup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// USD cents (avoid floating-point in persisted form).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usd_cents: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_ms: Option<u64>,
}

/// Optional Ed25519 signature over the attestation hash. Verifying
/// it is the consumer's job; the data layer just stores the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Hex-encoded Ed25519 public key.
    pub public_key: String,
    /// Hex-encoded signature over the lowercase-hex `attestation_id`.
    pub signature: String,
}

/// The persisted attestation. See module docs for what each field
/// is, what's in the hash, and what isn't.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub attestation_id: AttestationId,
    pub stage_id: StageId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_id: Option<OpId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<IntentId>,
    pub kind: AttestationKind,
    pub result: AttestationResult,
    pub produced_by: ProducerDescriptor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Wall-clock seconds since epoch when this attestation was
    /// produced. Excluded from `attestation_id` so the dedup
    /// property holds across runs.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

impl Attestation {
    /// Build an attestation against a stage, computing its
    /// content-addressed id. `timestamp` defaults to the current
    /// wall clock; pass to [`Attestation::with_timestamp`] in tests.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stage_id: impl Into<StageId>,
        op_id: Option<OpId>,
        intent_id: Option<IntentId>,
        kind: AttestationKind,
        result: AttestationResult,
        produced_by: ProducerDescriptor,
        cost: Option<Cost>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::with_timestamp(stage_id, op_id, intent_id, kind, result, produced_by, cost, now)
    }

    /// Build an attestation with a caller-controlled `timestamp`.
    /// Used in tests to keep golden hashes stable.
    #[allow(clippy::too_many_arguments)]
    pub fn with_timestamp(
        stage_id: impl Into<StageId>,
        op_id: Option<OpId>,
        intent_id: Option<IntentId>,
        kind: AttestationKind,
        result: AttestationResult,
        produced_by: ProducerDescriptor,
        cost: Option<Cost>,
        timestamp: u64,
    ) -> Self {
        let stage_id = stage_id.into();
        let attestation_id = compute_attestation_id(
            &stage_id,
            op_id.as_deref(),
            intent_id.as_deref(),
            &kind,
            &result,
            &produced_by,
        );
        Self {
            attestation_id,
            stage_id,
            op_id,
            intent_id,
            kind,
            result,
            produced_by,
            cost,
            timestamp,
            signature: None,
        }
    }

    /// Attach a signature. The signature is not part of the hash;
    /// the same logical attestation produced by an unsigned harness
    /// dedupes against a signed one. Callers who *want* signature
    /// to be part of identity should hash signature into the
    /// `produced_by.tool` string explicitly.
    pub fn with_signature(mut self, signature: Signature) -> Self {
        self.signature = Some(signature);
        self
    }
}

fn compute_attestation_id(
    stage_id: &str,
    op_id: Option<&str>,
    intent_id: Option<&str>,
    kind: &AttestationKind,
    result: &AttestationResult,
    produced_by: &ProducerDescriptor,
) -> AttestationId {
    let view = CanonicalAttestationView {
        stage_id,
        op_id,
        intent_id,
        kind,
        result,
        produced_by,
    };
    canonical::hash(&view)
}

/// Hashable shadow of [`Attestation`] omitting the fields we
/// deliberately exclude from identity (`attestation_id`, `cost`,
/// `timestamp`, `signature`). Lives only as a transient.
#[derive(Serialize)]
struct CanonicalAttestationView<'a> {
    stage_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent_id: Option<&'a str>,
    kind: &'a AttestationKind,
    result: &'a AttestationResult,
    produced_by: &'a ProducerDescriptor,
}

// ---- Persistence -------------------------------------------------

/// Persistent log of [`Attestation`] records.
///
/// Mirrors [`crate::OpLog`] / [`crate::IntentLog`] in shape: one
/// canonical-JSON file per attestation, atomic writes via tempfile +
/// rename, idempotent on re-puts. Adds a `by-stage/` index so "list
/// every attestation for stage X" is `O(attestations on X)` rather
/// than `O(all attestations)`.
pub struct AttestationLog {
    dir: PathBuf,
    by_stage: PathBuf,
}

impl AttestationLog {
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join("attestations");
        let by_stage = dir.join("by-stage");
        fs::create_dir_all(&by_stage)?;
        Ok(Self { dir, by_stage })
    }

    fn primary_path(&self, id: &AttestationId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Persist an attestation. Idempotent on existing ids — content
    /// addressing guarantees the same logical attestation produces
    /// the same id, so re-putting is a no-op for the primary file.
    /// The by-stage index is also re-written idempotently.
    pub fn put(&self, attestation: &Attestation) -> io::Result<()> {
        let primary = self.primary_path(&attestation.attestation_id);
        if !primary.exists() {
            let bytes = serde_json::to_vec(attestation)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let tmp = primary.with_extension("json.tmp");
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, &primary)?;
        }
        // Index entry: empty marker file. Reading the index is a
        // directory listing; resolving each entry is a primary-file
        // read by id.
        let stage_dir = self.by_stage.join(&attestation.stage_id);
        fs::create_dir_all(&stage_dir)?;
        let idx = stage_dir.join(&attestation.attestation_id);
        if !idx.exists() {
            fs::File::create(&idx)?;
        }
        Ok(())
    }

    pub fn get(&self, id: &AttestationId) -> io::Result<Option<Attestation>> {
        let path = self.primary_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let attestation: Attestation = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(attestation))
    }

    /// Enumerate attestations for a given stage. Order is not
    /// stable across calls (it follows directory iteration order).
    /// Callers that need a stable ordering should sort by
    /// `timestamp` or `attestation_id`.
    pub fn list_for_stage(&self, stage_id: &StageId) -> io::Result<Vec<Attestation>> {
        let stage_dir = self.by_stage.join(stage_id);
        if !stage_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&stage_dir)? {
            let entry = entry?;
            let id = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if let Some(att) = self.get(&id)? {
                out.push(att);
            }
        }
        Ok(out)
    }

}

// ---- Tests --------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ci_runner() -> ProducerDescriptor {
        ProducerDescriptor {
            tool: "lex check".into(),
            version: "0.1.0".into(),
            model: None,
        }
    }

    fn typecheck_passed() -> Attestation {
        Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            ci_runner(),
            None,
            1000,
        )
    }

    #[test]
    fn same_logical_verification_hashes_equal() {
        // Dedup invariant: same stage, same kind, same producer,
        // same outcome → same `attestation_id` regardless of
        // wall-clock or cost.
        let a = typecheck_passed();
        let b = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            ci_runner(),
            Some(Cost {
                tokens_in: Some(0),
                tokens_out: Some(0),
                usd_cents: Some(0),
                wall_time_ms: Some(42),
            }),
            99999,
        );
        assert_eq!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn different_stages_hash_differently() {
        let a = typecheck_passed();
        let b = Attestation::with_timestamp(
            "stage-XYZ",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            ci_runner(),
            None,
            1000,
        );
        assert_ne!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn different_op_ids_hash_differently() {
        let a = typecheck_passed();
        let b = Attestation::with_timestamp(
            "stage-abc",
            Some("op-XYZ".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            ci_runner(),
            None,
            1000,
        );
        assert_ne!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn different_intents_hash_differently() {
        let a = Attestation::with_timestamp(
            "stage-abc", None,
            Some("intent-A".into()),
            AttestationKind::TypeCheck, AttestationResult::Passed,
            ci_runner(), None, 1000,
        );
        let b = Attestation::with_timestamp(
            "stage-abc", None,
            Some("intent-B".into()),
            AttestationKind::TypeCheck, AttestationResult::Passed,
            ci_runner(), None, 1000,
        );
        assert_ne!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn different_kinds_hash_differently() {
        let a = typecheck_passed();
        let b = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::EffectAudit,
            AttestationResult::Passed,
            ci_runner(),
            None,
            1000,
        );
        assert_ne!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn passed_vs_failed_hash_differently() {
        // Critical: a Failed attestation must not collide with a
        // Passed one for the same logical verification. Otherwise
        // a flaky producer could overwrite the negative evidence
        // by re-running and getting Passed.
        let a = typecheck_passed();
        let b = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Failed { detail: "arity mismatch".into() },
            ci_runner(),
            None,
            1000,
        );
        assert_ne!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn different_producers_hash_differently() {
        let a = typecheck_passed();
        let mut other = ci_runner();
        other.tool = "third-party-runner".into();
        let b = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            other,
            None,
            1000,
        );
        assert_ne!(
            a.attestation_id, b.attestation_id,
            "an attestation from a different producer is a different fact",
        );
    }

    #[test]
    fn signature_is_excluded_from_hash() {
        // A signed and unsigned attestation of the same logical
        // fact must dedupe. Otherwise late-signing a record would
        // create two attestations that say the same thing.
        let a = typecheck_passed();
        let b = typecheck_passed().with_signature(Signature {
            public_key: "ed25519:fffe".into(),
            signature: "0xabcd".into(),
        });
        assert_eq!(a.attestation_id, b.attestation_id);
    }

    #[test]
    fn attestation_id_is_64_char_lowercase_hex() {
        let a = typecheck_passed();
        assert_eq!(a.attestation_id.len(), 64);
        assert!(a
            .attestation_id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    #[test]
    fn round_trip_through_serde_json() {
        let a = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            Some("intent-A".into()),
            AttestationKind::Spec {
                spec_id: "clamp.spec".into(),
                method: SpecMethod::Random,
                trials: Some(1000),
            },
            AttestationResult::Passed,
            ProducerDescriptor {
                tool: "lex agent-tool".into(),
                version: "0.1.0".into(),
                model: Some("claude-opus-4-7".into()),
            },
            Some(Cost {
                tokens_in: Some(1234),
                tokens_out: Some(567),
                usd_cents: Some(2),
                wall_time_ms: Some(3400),
            }),
            99,
        )
        .with_signature(Signature {
            public_key: "ed25519:abc".into(),
            signature: "0x1234".into(),
        });
        let json = serde_json::to_string(&a).unwrap();
        let back: Attestation = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    /// Golden hash. If this changes, the canonical form has shifted
    /// — every `AttestationId` in every existing store has changed
    /// too. Update with care; same protective shape as the
    /// `Operation` and `Intent` golden tests.
    #[test]
    fn canonical_form_is_stable_for_a_known_input() {
        let a = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            ProducerDescriptor {
                tool: "lex check".into(),
                version: "0.1.0".into(),
                model: None,
            },
            None,
            0,
        );
        assert_eq!(
            a.attestation_id,
            "a4ef921f7bb0db70779c5b698cda1744d49165a4a56aa8414bdbafc85bcbc16b",
            "canonical-form regression: the AttestationId for a known input changed",
        );
    }

    // ---- AttestationLog ----

    #[test]
    fn log_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AttestationLog::open(tmp.path()).unwrap();
        let a = typecheck_passed();
        log.put(&a).unwrap();
        let read_back = log.get(&a.attestation_id).unwrap().unwrap();
        assert_eq!(a, read_back);
    }

    #[test]
    fn log_get_unknown_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AttestationLog::open(tmp.path()).unwrap();
        assert!(log
            .get(&"nonexistent".to_string())
            .unwrap()
            .is_none());
    }

    #[test]
    fn log_put_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AttestationLog::open(tmp.path()).unwrap();
        let a = typecheck_passed();
        log.put(&a).unwrap();
        log.put(&a).unwrap();
        let read_back = log.get(&a.attestation_id).unwrap().unwrap();
        assert_eq!(a, read_back);
    }

    #[test]
    fn list_for_stage_returns_only_that_stage() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AttestationLog::open(tmp.path()).unwrap();

        let on_abc_1 = typecheck_passed();
        let on_abc_2 = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::EffectAudit,
            AttestationResult::Passed,
            ci_runner(),
            None,
            2000,
        );
        let on_xyz = Attestation::with_timestamp(
            "stage-xyz",
            Some("op-456".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Passed,
            ci_runner(),
            None,
            1000,
        );

        log.put(&on_abc_1).unwrap();
        log.put(&on_abc_2).unwrap();
        log.put(&on_xyz).unwrap();

        let mut on_abc = log.list_for_stage(&"stage-abc".to_string()).unwrap();
        on_abc.sort_by_key(|a| a.timestamp);
        assert_eq!(on_abc.len(), 2);
        assert_eq!(on_abc[0], on_abc_1);
        assert_eq!(on_abc[1], on_abc_2);

        let on_xyz_listed = log.list_for_stage(&"stage-xyz".to_string()).unwrap();
        assert_eq!(on_xyz_listed.len(), 1);
        assert_eq!(on_xyz_listed[0], on_xyz);
    }

    #[test]
    fn list_for_unknown_stage_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let log = AttestationLog::open(tmp.path()).unwrap();
        let v = log.list_for_stage(&"never-attested".to_string()).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn passed_and_failed_for_same_stage_both_persist() {
        // Failure attestations are evidence too; they must not be
        // overwritten by a later passing attestation. The hash
        // distinction (tested above) plus the by-stage listing
        // should keep both visible.
        let tmp = tempfile::tempdir().unwrap();
        let log = AttestationLog::open(tmp.path()).unwrap();

        let passed = typecheck_passed();
        let failed = Attestation::with_timestamp(
            "stage-abc",
            Some("op-123".into()),
            None,
            AttestationKind::TypeCheck,
            AttestationResult::Failed { detail: "arity mismatch".into() },
            ci_runner(),
            None,
            500,
        );

        log.put(&failed).unwrap();
        log.put(&passed).unwrap();

        let listing = log.list_for_stage(&"stage-abc".to_string()).unwrap();
        assert_eq!(listing.len(), 2, "both passing and failing evidence must persist");
    }
}
