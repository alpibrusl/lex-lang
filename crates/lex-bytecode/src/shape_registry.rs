//! Process-global record-shape registry (#462 slice 3).
//!
//! Maps sorted field-name lists to stable `u32` shape IDs for records
//! built at runtime — JSON decode, SQL row, HTTP body, host effect
//! handlers, builtin returns. Anywhere `Value::record_dynamic` is
//! called.
//!
//! Why: the slice-2b measurement (`docs/design/ic-polymorphism-measurement.md`)
//! found 14% of GetField IC hits — and 100% of inbox/gateway/std_http
//! traffic — landed on records carrying `NO_SHAPE_ID`. Those records
//! all aliased on the sentinel, so the IC's shape-keyed verification
//! (added in #517) couldn't distinguish them and fell through to the
//! name-compare path on every hit. With this registry, dynamic records
//! built from the same field set share a real `shape_id` and the IC
//! hits cleanly amongst them.
//!
//! ## Disjoint ID spaces
//!
//! Compile-time `Op::MakeRecord` shape IDs come from
//! `Program::record_shapes` and are per-program indices (0, 1, 2, …).
//! Runtime registry IDs come from this module and live in the high
//! half of the `u32` range starting at [`DYNAMIC_SHAPE_ID_BASE`].
//! `NO_SHAPE_ID` (`u32::MAX`) is reserved.
//!
//! Disjoint spaces are required for IC correctness: the shape-keyed
//! verifier in `vm.rs` treats `cached_shape == incoming_shape` as
//! "offset is sound" for non-`NO_SHAPE_ID` cases. If a compile-time
//! ID collided with a dynamic ID for a different field set, the IC
//! would return values from the wrong field.
//!
//! ## Cost
//!
//! One `RwLock<IndexMap>` lookup per `record_dynamic` call. The map
//! grows monotonically (shapes are never freed) but is small in
//! practice — tens of distinct shapes per workload. Lookups are
//! read-lock + hash; misses take the write lock once per new shape.
//!
//! ## Sorting
//!
//! The key is the sorted field-name vec — two records with the same
//! fields in different insertion order share a `shape_id`. Matches
//! the existing `Value::Record` `PartialEq` (which compares `IndexMap`
//! contents structurally, ignoring `shape_id`), so equality and IC
//! sharing line up.

use indexmap::IndexMap;
use std::sync::{OnceLock, RwLock};

/// Base for dynamically-interned shape IDs. Chosen so that
/// `Program::record_shapes` indices (which start at 0 and grow up)
/// cannot collide with dynamic IDs even for pathologically large
/// programs.  Compile-time records will hit a different IC slot
/// outcome than dynamic records with the same field set — that's
/// the cost of keeping ID spaces disjoint; the slice-2b
/// measurement found 0 mixed-flavor sites in real workloads.
pub const DYNAMIC_SHAPE_ID_BASE: u32 = 0x8000_0000;

fn registry() -> &'static RwLock<IndexMap<Vec<String>, u32>> {
    static REGISTRY: OnceLock<RwLock<IndexMap<Vec<String>, u32>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(IndexMap::new()))
}

/// Look up (or assign) the shape ID for the given field-name list.
/// Same set of names — in any insertion order — yields the same ID
/// for the lifetime of the process.
pub fn intern(field_names: impl IntoIterator<Item = impl AsRef<str>>) -> u32 {
    let mut key: Vec<String> = field_names.into_iter().map(|s| s.as_ref().to_string()).collect();
    key.sort();
    // Fast path: read-only lookup.
    if let Some(&id) = registry().read().unwrap().get(&key) {
        return id;
    }
    // Slow path: write-lock, re-check, insert.
    let mut w = registry().write().unwrap();
    if let Some(&id) = w.get(&key) {
        return id;
    }
    let id = DYNAMIC_SHAPE_ID_BASE + w.len() as u32;
    w.insert(key, id);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_fields_yield_same_id_regardless_of_order() {
        let a = intern(["x", "y", "z"]);
        let b = intern(["z", "y", "x"]);
        let c = intern(["y", "x", "z"]);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn distinct_fields_yield_distinct_ids() {
        let a = intern(["a", "b"]);
        let b = intern(["a", "c"]);
        assert_ne!(a, b);
    }

    #[test]
    fn ids_are_in_the_dynamic_range() {
        let id = intern(["only_for_this_test"]);
        assert!(id >= DYNAMIC_SHAPE_ID_BASE);
        assert!(id < u32::MAX);
    }
}
