//! Canonical JSON serialization for hashing.
//!
//! The contract is narrow: given any `Serialize` value, produce a byte
//! sequence such that two values that *should* hash equal produce
//! identical bytes. We get there by:
//!
//! 1. **Field order from the struct/enum declaration.** `serde_json`
//!    emits struct fields in the order they appear in the type. Since
//!    we control the operation enum, this gives us a stable layout
//!    without sorting at serialize time. The risk is that *changing*
//!    the type's field order silently rewrites every `OpId`; the
//!    operation tests below pin a few golden hashes so a refactor
//!    that breaks identity surfaces as a test failure.
//!
//! 2. **`BTreeSet` / `BTreeMap` for unordered collections.** Effect
//!    sets are `BTreeSet<String>` so iteration is sorted. If we ever
//!    add a map-shaped field (e.g. for variant payloads), it should
//!    use `BTreeMap` for the same reason.
//!
//! 3. **`serde_json` compact emit.** No pretty-printing, no trailing
//!    whitespace. The compact form is canonical because it has no
//!    formatting choices.
//!
//! The only thing we explicitly *don't* do here is recursive key
//! sorting on arbitrary `serde_json::Value` trees. We don't need it:
//! every type that contributes to an [`OpId`] is concrete and uses
//! one of the three patterns above. If you find yourself wanting to
//! hash a `serde_json::Value` directly, route it through a typed
//! struct first.

use sha2::{Digest, Sha256};

/// Hash a serializable value to a 64-char lowercase hex digest.
pub(crate) fn hash<T: serde::Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).expect("canonical serialization");
    let digest = Sha256::digest(&bytes);
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
