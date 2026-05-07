//! Stable binary canonical-AST format for Lex programs (#206).
//!
//! The text-format `.lex` source is a debugger / new-developer
//! affordance; the canonical AST (`Vec<Stage>`) is the substrate
//! that matters. This module gives that substrate a stable wire
//! representation so agents can submit canonical AST directly to
//! `lex-bytecode::compile_program` without round-tripping through
//! the parser, and so two agents proposing the same logical change
//! produce byte-identical input.
//!
//! Encoding:
//!   - byte 0: format version (currently `1`).
//!   - bytes 1..: canonical-JSON bytes of the `Vec<Stage>`, using
//!     the same `canon_json` rules `lex-vcs` already uses for OpId
//!     content-addressing. Object keys sorted, no whitespace, UTF-8.
//!
//! Why JSON-flavored bytes instead of CBOR or postcard:
//!
//! * `lex-vcs::OpId`, `StageId`, and `SigId` already hash via
//!   canonical-JSON. Reusing the same byte representation means
//!   "OpId is bit-identical across runs producing the same
//!   logical program from canonical-AST input" (the issue's
//!   acceptance criterion) holds by construction — no separate
//!   format to keep in sync.
//! * Adding a CBOR / postcard / etc. dep is deferred to a later
//!   slice once a need shows up. Today the agent-emit + compile
//!   round-trip works on these bytes; the format can swap behind
//!   `encode_program` / `decode_program` without breaking callers.
//!
//! Versioning rules:
//!
//! Adding a new `Stage` variant or a new field to an existing
//! variant doesn't bump the version — serde's default-value
//! handling reads old bytes into the new struct. Removing or
//! renaming a field DOES bump the version. Today's version is
//! `1`; if it ever bumps, `decode_program` keeps a thin shim
//! that recognises legacy bytes and runs the appropriate
//! migration.

use crate::canonical::Stage;

/// Current canonical-format version. Bumped on incompatible
/// schema changes (field removal/rename); additive changes
/// (new variants, new fields with defaults) stay version-stable.
pub const CANONICAL_VERSION: u8 = 1;

/// Errors `decode_program` surfaces on malformed input.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DecodeError {
    #[error("canonical-AST input is empty (no version byte)")]
    Empty,
    #[error("unsupported canonical-AST version: {found} (this build supports {supported})")]
    UnsupportedVersion { found: u8, supported: u8 },
    #[error("canonical-AST payload is not valid UTF-8: {0}")]
    NotUtf8(String),
    #[error("canonical-AST payload didn't deserialize: {0}")]
    Deserialize(String),
}

/// Encode a program (`Vec<Stage>`) to its canonical bytes.
///
/// Round-trip property: for any two parses `a` and `b` of the same
/// `.lex` source, `encode_program(&a) == encode_program(&b)`. And for
/// any program `s`, `decode_program(&encode_program(&s))` returns
/// `Ok(s')` with `encode_program(&s') == encode_program(&s)`.
pub fn encode_program(stages: &[Stage]) -> Vec<u8> {
    let value = serde_json::to_value(stages)
        .expect("Vec<Stage> is always JSON-serializable");
    let canonical = crate::canon_json::to_canonical_string(&value);
    let mut out = Vec::with_capacity(1 + canonical.len());
    out.push(CANONICAL_VERSION);
    out.extend_from_slice(canonical.as_bytes());
    out
}

/// Decode canonical bytes back to a program. Verifies the version
/// byte before attempting deserialization so a wrong-version input
/// surfaces as a clean error instead of a confusing serde failure.
pub fn decode_program(bytes: &[u8]) -> Result<Vec<Stage>, DecodeError> {
    let (version, payload) = bytes.split_first()
        .ok_or(DecodeError::Empty)?;
    if *version != CANONICAL_VERSION {
        return Err(DecodeError::UnsupportedVersion {
            found: *version,
            supported: CANONICAL_VERSION,
        });
    }
    let s = std::str::from_utf8(payload)
        .map_err(|e| DecodeError::NotUtf8(e.to_string()))?;
    serde_json::from_str(s)
        .map_err(|e| DecodeError::Deserialize(e.to_string()))
}
