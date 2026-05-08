//! Canonical JSON serialization for hashing — the V1 canonical form
//! that every existing `OpId` was computed under.
//!
//! # The V1 canonical form (authoritative spec)
//!
//! These are the rules every contributor to an `OpId` must follow.
//! Violating any of them silently rewrites every `OpId` in every
//! existing store. Issue #244 covers versioning the form so future
//! evolutions are explicit; until then, treat each rule as
//! load-bearing.
//!
//! 1. **Compact JSON** (`serde_json::to_vec`). No pretty-printing,
//!    no trailing whitespace. The compact form has no formatting
//!    choices, so two independent serializers produce the same
//!    bytes.
//! 2. **Field order from the struct/enum declaration.** `serde_json`
//!    emits struct fields in declaration order. Reordering an
//!    `OperationKind` variant's fields (or the `CanonicalView`
//!    struct's fields) rotates every `OpId`.
//! 3. **`BTreeSet` for unordered string sets.** [`crate::EffectSet`]
//!    is a `BTreeSet<String>` so iteration is sorted by string
//!    order. Any new set-shaped field must use `BTreeSet`; a
//!    `HashSet` is not acceptable.
//! 4. **`BTreeMap` for unordered key-value collections.**
//!    `StageTransition::Merge { entries }` is a
//!    `BTreeMap<SigId, Option<StageId>>`. Iteration is sorted by
//!    `SigId`, so insertion order is irrelevant. Any new map-shaped
//!    field must use `BTreeMap`.
//! 5. **`Vec<OpId>` parents are sorted and deduped before
//!    hashing.** [`crate::Operation::new`] does this at construction
//!    time; [`crate::Operation::canonical_bytes`] re-sorts via a
//!    transient `BTreeSet<&OpId>` so a hand-constructed
//!    `Operation { parents: vec![...] }` still hashes canonically.
//! 6. **Empty `parents` arrays are emitted in the canonical form.**
//!    This differs from the on-disk JSON shape (which skips empty
//!    `parents`) — see [`crate::Operation::canonical_bytes`] for the
//!    exact pre-image fed to SHA-256.
//! 7. **Optional fields use `skip_serializing_if = "Option::is_none"`
//!    so `None` is omitted entirely.** Adding a `Some(...)` value
//!    where `None` was rotates the `OpId`; that's intentional (see
//!    `Operation::with_intent`). Switching from `Option<T>` to a
//!    `T` with a default value is a canonical-form break.
//! 8. **SHA-256 to lowercase hex.** 64 ASCII chars; uppercase or
//!    truncation is a canonical-form break.
//!
//! The only thing this module deliberately *does not* do is
//! recursive key sorting on arbitrary `serde_json::Value` trees.
//! That's not necessary because every type that contributes to an
//! [`crate::OpId`] is concrete and uses one of the patterns above.
//! If you find yourself wanting to hash a `serde_json::Value`
//! directly, route it through a typed struct first.

use sha2::{Digest, Sha256};

/// Hash a serializable value to a 64-char lowercase hex digest.
///
/// Internal helper for places that hash typed structs directly.
/// New callers should prefer [`crate::Operation::canonical_bytes`]
/// + [`hash_bytes`] so the pre-image stays inspectable.
pub(crate) fn hash<T: serde::Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).expect("canonical serialization");
    hash_bytes(&bytes)
}

/// Hash an already-canonicalized byte sequence to lowercase hex.
pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::hash;
    use serde::Serialize;

    #[derive(Serialize)]
    struct Point { x: i32, y: i32 }

    #[test]
    fn identical_values_hash_equal() {
        let a = Point { x: 1, y: 2 };
        let b = Point { x: 1, y: 2 };
        assert_eq!(hash(&a), hash(&b));
    }

    #[test]
    fn different_values_hash_differently() {
        let a = Point { x: 1, y: 2 };
        let b = Point { x: 2, y: 1 };
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn hash_is_64_char_lowercase_hex() {
        let h = hash(&Point { x: 0, y: 0 });
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_lowercase())));
    }
}
