//! Delta encoding for stage canonical bytes (#261 slice 3).
//!
//! Each implementation lives at
//! `<root>/stages/<sig>/implementations/<stage_id>.ast.json` —
//! the canonical bytes of the AST, content-addressed by `stage_id`.
//! For most edits, two consecutive stages share a long byte prefix
//! and a long byte suffix (e.g. body changes inside the same fn
//! shape), so storing every stage as a full file is wasteful.
//!
//! Slice 3 ships an opt-in delta format: when a stage's diff
//! against an existing parent stage is below
//! [`DELTA_RATIO_THRESHOLD`], persist `<stage_id>.delta.json`
//! holding `(base_stage_id, common_prefix_len, common_suffix_len,
//! middle_bytes_hex)` instead of the full bytes. Reconstruction
//! splices `base_bytes[..prefix] + middle + base_bytes[tail..]`.
//!
//! Chain length is capped at [`DELTA_CHAIN_CAP`]; once a chain
//! reaches the cap, the next stage is materialized as a full
//! snapshot so reconstruction stays O(1) per access in the limit.
//!
//! # Determinism
//!
//! The delta format is *not* canonical — multiple `(prefix,
//! suffix, middle)` decompositions of the same byte change are
//! valid. We pick the largest common prefix, then the largest
//! suffix that doesn't overlap the prefix, so the format is
//! single-valued for a given `(base_bytes, new_bytes)` pair.

use serde::{Deserialize, Serialize};

/// Maximum length of a delta chain. Past this, [`encode`] yields
/// `None` so the caller writes a full snapshot. The cap is a
/// pragmatic balance between disk savings and reconstruction cost
/// — 32 keeps the worst-case `get_ast` at 32 file reads + 32
/// splices, which dominates over filesystem latency.
pub const DELTA_CHAIN_CAP: usize = 32;

/// A new stage is delta-encoded when the *middle* (non-shared)
/// bytes are at most this fraction of the new stage's size.
/// Below the threshold the delta is a clear win; above it, the
/// metadata overhead and the indirection cost of reconstruction
/// usually outweigh the byte savings.
pub const DELTA_RATIO_THRESHOLD: f64 = 0.5;

/// On-disk format of a delta-encoded stage.
///
/// File path: `<sig>/implementations/<stage_id>.delta.json`.
/// The pair `(common_prefix, common_suffix)` must satisfy
/// `prefix + suffix <= base_bytes.len()` — they describe a
/// non-overlapping splice into the base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageDelta {
    /// `stage_id` of the previous stage these bytes are diff'd
    /// against. Either holds full bytes (`.ast.json`) or its own
    /// delta — reconstruction recurses.
    pub base_stage_id: String,
    /// Length of the chain ending at *this* stage. A delta against
    /// a full snapshot has `chain_length: 1`; chained deltas
    /// increment from there.
    pub chain_length: usize,
    /// Number of bytes shared at the start with `base`.
    pub common_prefix: usize,
    /// Number of bytes shared at the end with `base`.
    pub common_suffix: usize,
    /// Lowercase-hex of the replacement bytes for the middle.
    /// Empty hex string means "delete the middle, splice prefix
    /// directly to suffix" — happens when the new bytes are a
    /// pure deletion against the base.
    pub middle_hex: String,
}

/// Compute the splice between `base` and `new`. Always succeeds —
/// degenerate inputs (identical bytes, total replacement) produce
/// valid but trivial deltas. The caller decides whether the
/// resulting middle is small enough to be worth storing as a
/// delta versus a full snapshot.
pub fn splice(base: &[u8], new: &[u8]) -> (usize, usize, Vec<u8>) {
    let prefix_len = common_prefix_len(base, new);
    // Suffix can't overlap the prefix in either side: cap at the
    // remaining length of each.
    let max_suffix = std::cmp::min(
        base.len().saturating_sub(prefix_len),
        new.len().saturating_sub(prefix_len),
    );
    let suffix_len = common_suffix_len(
        &base[base.len() - max_suffix..],
        &new[new.len() - max_suffix..],
    );
    let middle = new[prefix_len..new.len() - suffix_len].to_vec();
    (prefix_len, suffix_len, middle)
}

/// Apply a splice to `base`, producing the reconstructed `new`
/// bytes. Returns an error when the prefix+suffix lengths would
/// overflow the base — guards against tampered or corrupt
/// `.delta.json` files.
pub fn apply(base: &[u8], delta: &StageDelta) -> Result<Vec<u8>, DeltaError> {
    let middle = hex::decode(&delta.middle_hex)
        .map_err(|e| DeltaError::InvalidHex(e.to_string()))?;
    if delta.common_prefix + delta.common_suffix > base.len() {
        return Err(DeltaError::OverlappingSplice {
            base_len: base.len(),
            prefix: delta.common_prefix,
            suffix: delta.common_suffix,
        });
    }
    let prefix = &base[..delta.common_prefix];
    let suffix_start = base.len() - delta.common_suffix;
    let suffix = &base[suffix_start..];
    let mut out = Vec::with_capacity(prefix.len() + middle.len() + suffix.len());
    out.extend_from_slice(prefix);
    out.extend(middle);
    out.extend_from_slice(suffix);
    Ok(out)
}

/// Decide whether the delta is worth storing. `middle_len` is the
/// length of the splice's replacement bytes; `new_len` is the
/// total length of the new bytes. Returns `true` when the ratio
/// is below [`DELTA_RATIO_THRESHOLD`] *and* the chain length isn't
/// at the cap.
pub fn is_worth_encoding(middle_len: usize, new_len: usize, chain_length: usize) -> bool {
    if chain_length > DELTA_CHAIN_CAP {
        return false;
    }
    if new_len == 0 {
        return false;
    }
    (middle_len as f64) / (new_len as f64) < DELTA_RATIO_THRESHOLD
}

fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn common_suffix_len(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

#[derive(Debug, thiserror::Error)]
pub enum DeltaError {
    #[error("delta middle is not valid hex: {0}")]
    InvalidHex(String),
    #[error("delta splice overflows base: base_len={base_len}, prefix={prefix}, suffix={suffix}")]
    OverlappingSplice {
        base_len: usize,
        prefix: usize,
        suffix: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_then_apply_round_trips() {
        let base = b"hello, the quick brown fox jumps over the lazy dog";
        let new = b"hello, the quick green fox jumps over the lazy dog";
        let (p, s, m) = splice(base, new);
        let delta = StageDelta {
            base_stage_id: "x".into(),
            chain_length: 1,
            common_prefix: p,
            common_suffix: s,
            middle_hex: hex::encode(&m),
        };
        let reconstructed = apply(base, &delta).unwrap();
        assert_eq!(reconstructed, new);
    }

    #[test]
    fn identical_bytes_yield_empty_middle() {
        let base = b"unchanged";
        let new = b"unchanged";
        let (p, s, m) = splice(base, new);
        assert_eq!(p, 9);
        assert_eq!(s, 0);
        assert!(m.is_empty(), "no middle when bytes are identical");
        // Apply gives back base.
        let delta = StageDelta {
            base_stage_id: "x".into(),
            chain_length: 1,
            common_prefix: p,
            common_suffix: s,
            middle_hex: hex::encode(&m),
        };
        assert_eq!(apply(base, &delta).unwrap(), base);
    }

    #[test]
    fn pure_insertion_is_pure_middle() {
        let base = b"abXY";
        let new = b"abZZZZXY";
        let (p, s, m) = splice(base, new);
        assert_eq!(p, 2);
        assert_eq!(s, 2);
        assert_eq!(m, b"ZZZZ");
    }

    #[test]
    fn pure_deletion_yields_empty_middle() {
        let base = b"abZZZZXY";
        let new = b"abXY";
        let (p, s, m) = splice(base, new);
        assert_eq!(p, 2);
        assert_eq!(s, 2);
        assert!(m.is_empty());
        let delta = StageDelta {
            base_stage_id: "x".into(),
            chain_length: 1,
            common_prefix: p,
            common_suffix: s,
            middle_hex: hex::encode(&m),
        };
        assert_eq!(apply(base, &delta).unwrap(), new);
    }

    #[test]
    fn is_worth_encoding_respects_threshold() {
        // Middle is 30% of new — under 50% threshold.
        assert!(is_worth_encoding(30, 100, 1));
        // 60% — over threshold.
        assert!(!is_worth_encoding(60, 100, 1));
        // Chain length cap.
        assert!(!is_worth_encoding(1, 100, DELTA_CHAIN_CAP + 1));
    }

    #[test]
    fn apply_refuses_overlapping_splice() {
        let base = b"short";
        let delta = StageDelta {
            base_stage_id: "x".into(),
            chain_length: 1,
            common_prefix: 4,
            common_suffix: 4,  // 4 + 4 > 5 — invalid
            middle_hex: String::new(),
        };
        assert!(apply(base, &delta).is_err());
    }
}
