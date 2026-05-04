//! First-class `Intent` object linked to operations (#131).
//!
//! Today the op log records *what* changed (typed deltas on the
//! AST). Intent captures *why* — the prompt that caused an agent
//! to make the change, the model that interpreted it, and the
//! session that grouped it with sibling ops.
//!
//! This matters for two reasons:
//! 1. **Audit.** When an agent commits a regression, the maintainer
//!    needs the prompt that led to it. The commit message can be
//!    made up; the prompt is the actual causal event.
//! 2. **Coordination.** When multiple agents work in parallel,
//!    knowing which operations belong to which intent lets the
//!    harness group them — agent A's work on intent-X is
//!    independent of agent B's work on intent-Y.
//!
//! # Identity
//!
//! [`IntentId`] is the SHA-256 of the canonical form of
//! `(prompt, session_id, model, parent_intent)` — `created_at` is
//! deliberately *not* part of the hash, so two runs of the same
//! prompt at different times still dedupe. The
//! "same `(prompt, model, session)` → same `intent_id`" invariant
//! is what #131's audit story rests on.
//!
//! # Storage
//!
//! `<root>/intents/<IntentId>.json` — same shape as `<root>/ops/`
//! and `<root>/stages/`. Atomic writes via tempfile + rename;
//! idempotent on existing IDs.
//!
//! # Privacy boundary
//!
//! Prompts may contain sensitive data. Keeping intents in their
//! own addressable namespace (rather than inlining the prompt on
//! every op) makes per-intent ACLs tractable as a follow-up
//! without touching the op log itself.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::canonical;

/// Content-addressed identity of an intent. Lowercase-hex SHA-256
/// of the canonical form of `(prompt, session_id, model,
/// parent_intent)`. Excludes `created_at` so two runs of the same
/// prompt produce the same id.
pub type IntentId = String;

/// Groups intents from the same agent session. Free-form string
/// so callers can use whatever session model their harness has.
pub type SessionId = String;

/// Which model produced the intent. Tracked so audit / blame can
/// answer "what model wrote this?" without joining against an
/// external table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Vendor / origin: `"anthropic"`, `"openai"`, `"local"`, etc.
    pub provider: String,
    /// The model name: `"claude-opus-4-7"`, `"gpt-5"`, etc.
    pub name: String,
    /// Optional version pin. `None` means "whatever the provider
    /// served"; `Some("2026-04-01")` lets the harness record an
    /// exact API revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The persisted intent. Carries the prompt that caused some
/// operations to be produced, the model that interpreted it, and
/// the session that grouped them. Many ops can share one intent;
/// duplicating the prompt on each would be wasteful and break the
/// "two equal ops hash equal" invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub intent_id: IntentId,
    pub prompt: String,
    pub session_id: SessionId,
    pub model: ModelDescriptor,
    /// For refinement chains ("the user said X, then said 'now also
    /// handle Y'"). `None` for top-level intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_intent: Option<IntentId>,
    /// Wall-clock seconds since epoch when this intent was first
    /// created. Excluded from `intent_id` so the dedup property
    /// holds across runs.
    pub created_at: u64,
}

impl Intent {
    /// Build an intent and compute its content-addressed id.
    /// `created_at` is filled in from the current wall clock; pass
    /// to [`Intent::with_timestamp`] if you want to control it
    /// explicitly (e.g. in tests).
    pub fn new(
        prompt: impl Into<String>,
        session_id: impl Into<SessionId>,
        model: ModelDescriptor,
        parent_intent: Option<IntentId>,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::with_timestamp(prompt, session_id, model, parent_intent, now)
    }

    /// Build an intent with a caller-controlled `created_at`. Used
    /// in tests to keep golden hashes stable; production code uses
    /// [`Intent::new`].
    pub fn with_timestamp(
        prompt: impl Into<String>,
        session_id: impl Into<SessionId>,
        model: ModelDescriptor,
        parent_intent: Option<IntentId>,
        created_at: u64,
    ) -> Self {
        let prompt = prompt.into();
        let session_id = session_id.into();
        let intent_id = compute_intent_id(&prompt, &session_id, &model, parent_intent.as_deref());
        Self {
            intent_id,
            prompt,
            session_id,
            model,
            parent_intent,
            created_at,
        }
    }
}

fn compute_intent_id(
    prompt: &str,
    session_id: &str,
    model: &ModelDescriptor,
    parent_intent: Option<&str>,
) -> IntentId {
    let view = CanonicalIntentView {
        prompt,
        session_id,
        model,
        parent_intent,
    };
    canonical::hash(&view)
}

/// Hashable shadow of [`Intent`] omitting `intent_id` (we're
/// computing it) and `created_at` (timestamp drift would break
/// dedup). Lives only as a transient for hashing.
#[derive(Serialize)]
struct CanonicalIntentView<'a> {
    prompt: &'a str,
    session_id: &'a str,
    model: &'a ModelDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_intent: Option<&'a str>,
}

// ---- Persistence -------------------------------------------------

/// Persistent log of [`Intent`] records. Mirrors [`crate::OpLog`]'s
/// shape: one canonical-JSON file per intent, atomic writes via
/// tempfile + rename, idempotent on re-puts.
pub struct IntentLog {
    dir: PathBuf,
}

impl IntentLog {
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join("intents");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, id: &IntentId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Persist an intent. Idempotent on existing ids — the bytes
    /// must match by content addressing, so re-putting the same
    /// intent is a no-op.
    pub fn put(&self, intent: &Intent) -> io::Result<()> {
        let path = self.path(&intent.intent_id);
        if path.exists() {
            return Ok(());
        }
        let bytes = serde_json::to_vec(intent)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, id: &IntentId) -> io::Result<Option<Intent>> {
        let path = self.path(id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let intent: Intent = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Some(intent))
    }
}

// ---- Tests --------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic() -> ModelDescriptor {
        ModelDescriptor {
            provider: "anthropic".into(),
            name: "claude-opus-4-7".into(),
            version: None,
        }
    }

    #[test]
    fn same_prompt_session_model_hashes_equal() {
        // The load-bearing dedup invariant: the same logical
        // intent (same prompt, same session, same model) should
        // produce the same `intent_id` regardless of which agent
        // session re-recorded it. `created_at` differs but is not
        // in the hash.
        let a = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 1000,
        );
        let b = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 99999,
        );
        assert_eq!(a.intent_id, b.intent_id);
        assert_ne!(a.created_at, b.created_at);
    }

    #[test]
    fn different_prompts_hash_differently() {
        let a = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 0,
        );
        let b = Intent::with_timestamp(
            "fix the cache bug", "ses_abc", anthropic(), None, 0,
        );
        assert_ne!(a.intent_id, b.intent_id);
    }

    #[test]
    fn different_sessions_hash_differently() {
        let a = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 0,
        );
        let b = Intent::with_timestamp(
            "fix the auth bug", "ses_xyz", anthropic(), None, 0,
        );
        assert_ne!(a.intent_id, b.intent_id);
    }

    #[test]
    fn different_models_hash_differently() {
        let a = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 0,
        );
        let mut model = anthropic();
        model.name = "claude-sonnet-4-6".into();
        let b = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", model, None, 0,
        );
        assert_ne!(a.intent_id, b.intent_id);
    }

    #[test]
    fn refinement_chain_distinguishes_parent_intent() {
        let a = Intent::with_timestamp(
            "now also handle Y", "ses_abc", anthropic(), None, 0,
        );
        let b = Intent::with_timestamp(
            "now also handle Y", "ses_abc", anthropic(),
            Some("parent-intent-id".into()), 0,
        );
        assert_ne!(
            a.intent_id, b.intent_id,
            "an intent with a parent is causally distinct from one without",
        );
    }

    #[test]
    fn intent_id_is_64_char_lowercase_hex() {
        let i = Intent::with_timestamp(
            "test", "ses_abc", anthropic(), None, 0,
        );
        assert_eq!(i.intent_id.len(), 64);
        assert!(i.intent_id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    #[test]
    fn round_trip_through_serde_json() {
        let i = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(),
            Some("parent".into()), 12345,
        );
        let json = serde_json::to_string(&i).unwrap();
        let back: Intent = serde_json::from_str(&json).unwrap();
        assert_eq!(i, back);
    }

    /// Golden hash. If this changes, the canonical form has shifted
    /// — every `IntentId` in every existing store has changed too.
    /// That's a major-version event for the data model and should
    /// be a deliberate decision; update with care. Same protective
    /// shape as the operation.rs golden test.
    #[test]
    fn canonical_form_is_stable_for_a_known_input() {
        let i = Intent::with_timestamp(
            "fix the auth bug",
            "ses_abc",
            ModelDescriptor {
                provider: "anthropic".into(),
                name: "claude-opus-4-7".into(),
                version: None,
            },
            None,
            0,
        );
        assert_eq!(
            i.intent_id,
            "5ede62683a249cd00afff49fdf56e8f659fe878a668c8b61e36f5fbc1de7c734",
        );
    }

    // ---- IntentLog ----

    #[test]
    fn intent_log_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let log = IntentLog::open(tmp.path()).unwrap();
        let i = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 100,
        );
        log.put(&i).unwrap();
        let read_back = log.get(&i.intent_id).unwrap().unwrap();
        assert_eq!(i, read_back);
    }

    #[test]
    fn intent_log_get_unknown_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let log = IntentLog::open(tmp.path()).unwrap();
        assert!(log.get(&"nonexistent".to_string()).unwrap().is_none());
    }

    #[test]
    fn intent_log_put_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let log = IntentLog::open(tmp.path()).unwrap();
        let i = Intent::with_timestamp(
            "fix the auth bug", "ses_abc", anthropic(), None, 100,
        );
        log.put(&i).unwrap();
        // Second put with the same content is a no-op (the file
        // already exists; content addressing guarantees the bytes
        // match).
        log.put(&i).unwrap();
        let read_back = log.get(&i.intent_id).unwrap().unwrap();
        assert_eq!(i, read_back);
    }
}
