//! Pure-function memoization (#229 adaptive, #461 follow-up): the
//! per-function warm-up state that decides whether a callee keeps
//! probing the cache, the argument-size bound that skips hashing
//! large payloads, and the structural 128-bit key hash over `Value`
//! arguments. The cache itself lives on `Vm` (see `super`); the
//! dispatch loop consults these on every effect-free call.

use super::*;

/// Adaptive-memoization warmup window (#229 adaptive). A pure
/// function is given this many cache-probing calls to demonstrate a
/// hit; if it reaches the window with zero hits, memoization is
/// disabled for it (its calls stop hashing args). A function that
/// genuinely benefits — e.g. naive recursive `fib`, where each call
/// immediately reuses sub-results — accumulates hits well before the
/// window closes and stays enabled. 64 balances "give real reuse a
/// chance" against "don't pay the hash forever on always-miss code".
pub(super) const MEMO_WARMUP_CALLS: u32 = 64;

/// Largest argument payload (Str / Bytes bytes plus one per nested
/// value) a pure call may carry and still be memoized (#764). The
/// memo key hashes every argument in full, so a call is charged the
/// size of its arguments whether or not the cache ever hits. The
/// adaptive gate above only retires a function that never hits; a
/// scanner helper like `char_at(src, p)` over a 300 KB document
/// hits once early (the same position is peeked twice), stays
/// enabled, and then pays 300 KB of hashing per character, which
/// turns a linear walk into a quadratic one. Calls over this budget
/// take the plain path; nothing that large repeats often enough
/// for the cache to pay for itself.
pub(super) const MEMO_MAX_ARG_BYTES: usize = 4096;

/// Does hashing `args` cost more than `budget`? Stops walking as soon
/// as the answer is yes, so the check itself is bounded by `budget`.
pub(super) fn memo_args_exceed(args: &[Value], budget: usize) -> bool {
    fn walk(v: &Value, remaining: &mut usize) -> bool {
        let cost = match v {
            Value::Str(s) => s.len() + 1,
            Value::Bytes(b) => b.len() + 1,
            _ => 1,
        };
        if cost > *remaining {
            return true;
        }
        *remaining -= cost;
        match v {
            Value::List(items) => items.iter().any(|x| walk(x, remaining)),
            Value::Tuple(items) => items.iter().any(|x| walk(x, remaining)),
            Value::Record { fields, .. } => fields.values().any(|x| walk(x, remaining)),
            Value::Map(m) => m.values().any(|x| walk(x, remaining)),
            Value::Variant { args, .. } => args.iter().any(|x| walk(x, remaining)),
            _ => false,
        }
    }
    let mut remaining = budget;
    args.iter().any(|a| walk(a, &mut remaining))
}

/// Per-function adaptive-memoization state (#229 adaptive). `enabled`
/// starts true; once a function reaches `MEMO_WARMUP_CALLS` cache
/// probes with `hits == 0`, it flips to false and that function's
/// calls skip the args hash entirely for the rest of the Vm's life.
#[derive(Clone, Copy)]
pub(super) struct MemoFnState {
    pub(super) calls: u32,
    pub(super) hits: u32,
    pub(super) enabled: bool,
}

impl Default for MemoFnState {
    fn default() -> Self {
        MemoFnState { calls: 0, hits: 0, enabled: true }
    }
}
/// Hash the argument list for a pure-fn memoization lookup (#229).
///
/// The memo cache (`pure_memo`) is keyed on this 128-bit digest with
/// no secondary equality check, so the contract is: argument lists
/// that are equal under `Value`'s `PartialEq` must produce the same
/// digest, and the 128-bit width keeps the false-collision rate
/// (which would return a wrong cached result) negligible.
///
/// History (#461 follow-up): this used to build a `serde_json::Value`
/// of every arg, canonicalize it, and SHA-256 the bytes. Profiling
/// the `response_build` workload showed that path at 27.6% of all
/// instructions — it dominated the VM, since every effect-free call
/// pays it whether or not the cache ever hits. The cache is per-`Vm`
/// and ephemeral, so a cryptographic, cross-process-stable key was
/// never needed. We now walk the `Value` tree directly into two
/// domain-separated `SipHash` passes (deterministic fixed-key
/// `DefaultHasher`), concatenating the two 64-bit outputs into a
/// 128-bit key. No JSON allocation, no crypto.
///
/// The walk mirrors `Value::PartialEq` so the equal-args-equal-key
/// contract holds: `Record` is hashed order-independently over its
/// fields (matching `IndexMap`'s order-insensitive equality),
/// `Closure` on `(body_hash, captures)` not `fn_id` (#222), and
/// `Actor`/`Ticker` on pointer identity (matching `Arc::ptr_eq`).
pub(super) fn hash_call_args(args: &[Value]) -> [u8; 16] {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut h0 = DefaultHasher::new();
    let mut h1 = DefaultHasher::new();
    // Domain separator: makes the two passes diverge so the
    // concatenated halves span the full 128-bit space rather than
    // duplicating one 64-bit value.
    h1.write_u8(0x9e);
    h0.write_usize(args.len());
    h1.write_usize(args.len());
    for a in args {
        hash_value_into(a, &mut h0);
        hash_value_into(a, &mut h1);
    }
    let lo = h0.finish();
    let hi = h1.finish();
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    out
}

/// Structural hash of a `Value` into `h`, consistent with
/// `Value::PartialEq`. The leading discriminant byte keeps distinct
/// variants from colliding (e.g. `Int(0)` vs `Bool(false)`).
pub(super) fn hash_value_into<H: std::hash::Hasher>(v: &Value, h: &mut H) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher as _;
    match v {
        Value::Int(n) => { h.write_u8(0x01); h.write_i64(*n); }
        // Bit pattern, not value: total and deterministic. NaN==NaN
        // by bits (a memo hit there is harmless — the callee is pure
        // and returns the same result for bit-identical args), and
        // +0.0/-0.0 differ (a harmless extra miss).
        Value::Float(f) => { h.write_u8(0x02); h.write_u64(f.to_bits()); }
        Value::Bool(b) => { h.write_u8(0x03); h.write_u8(*b as u8); }
        Value::Str(s) => {
            h.write_u8(0x04);
            h.write_usize(s.len());
            h.write(s.as_bytes());
        }
        Value::Bytes(b) => {
            h.write_u8(0x05);
            h.write_usize(b.len());
            h.write(b);
        }
        Value::Unit => { h.write_u8(0x06); }
        Value::List(items) => {
            h.write_u8(0x07);
            h.write_usize(items.len());
            for it in items { hash_value_into(it, h); }
        }
        Value::Tuple(items) => {
            h.write_u8(0x08);
            h.write_usize(items.len());
            for it in items { hash_value_into(it, h); }
        }
        Value::Deque(items) => {
            h.write_u8(0x09);
            h.write_usize(items.len());
            for it in items { hash_value_into(it, h); }
        }
        // `IndexMap` equality is order-insensitive, so the hash must
        // be too: combine per-entry sub-hashes with wrapping add (a
        // commutative mix) rather than feeding them in iteration
        // order.
        Value::Record { fields, .. } => {
            h.write_u8(0x0a);
            let mut combined: u64 = 0;
            for (k, val) in fields.iter() {
                let mut e = DefaultHasher::new();
                e.write(k.as_bytes());
                e.write_u8(0xff);
                hash_value_into(val, &mut e);
                combined = combined.wrapping_add(e.finish());
            }
            h.write_u64(combined);
            h.write_usize(fields.len());
        }
        Value::Variant { name, args } => {
            h.write_u8(0x0b);
            h.write_usize(name.len());
            h.write(name.as_bytes());
            h.write_usize(args.len());
            for a in args { hash_value_into(a, h); }
        }
        // Identity is `(body_hash, captures)`, not `fn_id` (#222).
        Value::Closure { body_hash, captures, .. } => {
            h.write_u8(0x0c);
            h.write(body_hash);
            h.write_usize(captures.len());
            for c in captures { hash_value_into(c, h); }
        }
        Value::F64Array { rows, cols, data } => {
            h.write_u8(0x0d);
            h.write_u32(*rows);
            h.write_u32(*cols);
            for f in data { h.write_u64(f.to_bits()); }
        }
        // BTreeMap / BTreeSet iterate in sorted key order — already
        // canonical, so direct feed is order-independent.
        Value::Map(m) => {
            h.write_u8(0x0e);
            h.write_usize(m.len());
            for (k, val) in m {
                hash_mapkey_into(k, h);
                hash_value_into(val, h);
            }
        }
        Value::Set(s) => {
            h.write_u8(0x0f);
            h.write_usize(s.len());
            for k in s { hash_mapkey_into(k, h); }
        }
        // Pointer identity, matching `Arc::ptr_eq` in PartialEq.
        Value::Actor(a) => {
            h.write_u8(0x10);
            h.write_usize(Arc::as_ptr(a) as *const () as usize);
        }
        Value::Ticker(t) => {
            h.write_u8(0x11);
            h.write_usize(Arc::as_ptr(t) as *const () as usize);
        }
        // Coarse summary (schema + dimensions), matching the prior
        // `to_json` encoding which deliberately omitted the cell data
        // (tables can be GB-scale). Equal tables share schema + dims
        // so equal-args-equal-key holds; this is no coarser than the
        // pre-#461-followup behavior.
        Value::ArrowTable(t) => {
            h.write_u8(0x12);
            h.write_i64(t.num_rows() as i64);
            h.write_i64(t.num_columns() as i64);
            for f in t.schema().fields() {
                h.write(f.name().as_bytes());
                h.write_u8(0xfe);
            }
        }
        // #464: a StackRecord crossing into the memo path means an
        // escape the analysis was supposed to reject. Mirror the
        // PartialEq / to_json panic rather than mint a bogus key.
        Value::StackRecord { .. } =>
            panic!("BUG(#464): Value::StackRecord reached memo hashing — \
                    escape analysis should have prevented escape to a call boundary"),
        Value::StackTuple { .. } =>
            panic!("BUG(#464): Value::StackTuple reached memo hashing — \
                    escape analysis should have prevented escape to a call boundary"),
        // #463 slice 2a: arena handles must never reach memo hashing.
        // The memo cache outlives every request scope, so a hashed
        // arena handle would dangle. Slice 1's arena-eligibility
        // analysis must exclude pure-fn allocation sites (the memo
        // path is reached only through pure-fn calls) — any reach
        // here is a soundness bug.
        Value::ArenaRecord { .. } =>
            panic!("BUG(#463): Value::ArenaRecord reached memo hashing — \
                    arena-eligibility analysis must exclude pure-fn allocation sites"),
        Value::ArenaTuple { .. } =>
            panic!("BUG(#463): Value::ArenaTuple reached memo hashing — \
                    arena-eligibility analysis must exclude pure-fn allocation sites"),
    }
}

/// Hash a `MapKey` into `h` with its own discriminant so a `Str`
/// key and an `Int` key never collide.
pub(super) fn hash_mapkey_into<H: std::hash::Hasher>(k: &crate::value::MapKey, h: &mut H) {
    use crate::value::MapKey;
    match k {
        MapKey::Str(s) => { h.write_u8(0x01); h.write_usize(s.len()); h.write(s.as_bytes()); }
        MapKey::Int(n) => { h.write_u8(0x02); h.write_i64(*n); }
    }
}
#[cfg(test)]
mod memo_hash_tests {
    //! #461 follow-up: invariants for the structural memo-key hash
    //! that replaced the SHA-256-over-canonical-JSON path. The memo
    //! cache keys on this digest with no equality fallback, so the
    //! load-bearing property is "equal-under-PartialEq args produce
    //! an equal key" — plus enough discrimination that distinct args
    //! don't collide in practice.
    use super::*;
    use indexmap::IndexMap;

    fn rec(pairs: &[(&str, Value)]) -> Value {
        let mut m: IndexMap<SmolStr, Value> = IndexMap::new();
        for (k, v) in pairs { m.insert((*k).into(), v.clone()); }
        Value::Record { shape_id: crate::value::NO_SHAPE_ID, fields: Box::new(m) }
    }

    #[test]
    fn identical_args_hash_equal() {
        let a = vec![Value::Int(7), Value::Str("hi".into())];
        let b = vec![Value::Int(7), Value::Str("hi".into())];
        assert_eq!(hash_call_args(&a), hash_call_args(&b));
    }

    #[test]
    fn distinct_scalars_differ() {
        assert_ne!(hash_call_args(&[Value::Int(7)]), hash_call_args(&[Value::Int(8)]));
        assert_ne!(hash_call_args(&[Value::Int(0)]), hash_call_args(&[Value::Bool(false)]));
        assert_ne!(hash_call_args(&[Value::Int(0)]), hash_call_args(&[Value::Unit]));
        assert_ne!(hash_call_args(&[Value::Bool(true)]), hash_call_args(&[Value::Bool(false)]));
    }

    #[test]
    fn arity_is_part_of_the_key() {
        assert_ne!(
            hash_call_args(&[Value::Int(1), Value::Int(2)]),
            hash_call_args(&[Value::Int(1)]),
        );
        // A 2-arg call vs a single Tuple arg of the same elements
        // must not collide.
        assert_ne!(
            hash_call_args(&[Value::Int(1), Value::Int(2)]),
            hash_call_args(&[Value::Tuple(vec![Value::Int(1), Value::Int(2)])]),
        );
    }

    #[test]
    fn record_hash_is_field_order_independent() {
        // IndexMap equality ignores insertion order, so the key must
        // too — otherwise equal records would miss the cache.
        let r1 = rec(&[("a", Value::Int(1)), ("b", Value::Int(2))]);
        let r2 = rec(&[("b", Value::Int(2)), ("a", Value::Int(1))]);
        assert_eq!(r1, r2, "precondition: records compare equal");
        assert_eq!(hash_call_args(&[r1]), hash_call_args(&[r2]));
    }

    #[test]
    fn record_distinguishes_values_and_keys() {
        let base = rec(&[("a", Value::Int(1)), ("b", Value::Int(2))]);
        let diff_val = rec(&[("a", Value::Int(1)), ("b", Value::Int(3))]);
        let diff_key = rec(&[("a", Value::Int(1)), ("c", Value::Int(2))]);
        assert_ne!(hash_call_args(std::slice::from_ref(&base)), hash_call_args(&[diff_val]));
        assert_ne!(hash_call_args(&[base]), hash_call_args(&[diff_key]));
    }

    #[test]
    fn shape_id_does_not_affect_record_key() {
        // PartialEq ignores shape_id; the key must too.
        let mut m: IndexMap<SmolStr, Value> = IndexMap::new();
        m.insert("a".into(), Value::Int(1));
        let r_no_shape = Value::Record { shape_id: crate::value::NO_SHAPE_ID, fields: Box::new(m.clone()) };
        let r_shaped = Value::Record { shape_id: 3, fields: Box::new(m) };
        assert_eq!(r_no_shape, r_shaped);
        assert_eq!(hash_call_args(&[r_no_shape]), hash_call_args(&[r_shaped]));
    }

    #[test]
    fn variant_name_and_args_matter() {
        let some1 = Value::Variant { name: "Some".into(), args: vec![Value::Int(1)] };
        let some1b = Value::Variant { name: "Some".into(), args: vec![Value::Int(1)] };
        let some2 = Value::Variant { name: "Some".into(), args: vec![Value::Int(2)] };
        let none = Value::Variant { name: "None".into(), args: vec![] };
        assert_eq!(hash_call_args(std::slice::from_ref(&some1)), hash_call_args(&[some1b]));
        assert_ne!(hash_call_args(std::slice::from_ref(&some1)), hash_call_args(&[some2]));
        assert_ne!(hash_call_args(&[some1]), hash_call_args(&[none]));
    }

    #[test]
    fn float_bit_pattern_keys() {
        assert_eq!(hash_call_args(&[Value::Float(1.5)]), hash_call_args(&[Value::Float(1.5)]));
        assert_ne!(hash_call_args(&[Value::Float(1.5)]), hash_call_args(&[Value::Float(2.5)]));
        // Same NaN bit pattern → same key (harmless: pure callee is
        // deterministic on bit-identical args).
        let nan = f64::NAN;
        assert_eq!(hash_call_args(&[Value::Float(nan)]), hash_call_args(&[Value::Float(nan)]));
    }
}
