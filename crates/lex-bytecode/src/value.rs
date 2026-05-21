//! Runtime values.

use crate::program::BodyHash;
use arrow_array::RecordBatch;
use indexmap::IndexMap;
use smol_str::SmolStr;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Internal state of a `conc.Actor`. Protected by a `Mutex` so that
/// the `Lex` handler variant serialises on message delivery (one
/// message processed at a time, state mutated under the lock). The
/// `handler` is dispatched on the *calling* VM's thread — no extra
/// OS thread required — which lets Lex handlers invoke arbitrary
/// effects (sql, net, …) through the same handler chain.
///
/// Serialisation note: the `Native` variant releases the mutex
/// *before* invoking its closure (`state` is unused for natives —
/// the "state" is an external resource like a channel), so two
/// concurrent `conc.tell`s on the same native bridge may invoke
/// the closure on overlapping threads. Native bridges therefore
/// need to be internally thread-safe; the `serve_ws_fn_actor`
/// `mpsc::Sender` bridge is, because `Sender::send` is.
#[derive(Debug, Clone)]
pub struct ActorCell {
    pub state: Value,
    pub handler: ActorHandler,
}

/// Two ways an actor's handler can be implemented.
///
/// * `Lex(Value::Closure)` is the user-spawned shape from
///   `conc.spawn(state, fn (s, m) -> (s, r) { … })`. The VM calls
///   the closure with `(state, msg)` and expects `(new_state, reply)`.
///
/// * `Native(...)` is a Rust-side bridge — the actor cell wraps a
///   `Box<dyn Fn(Value) -> Result<Value, String>>` that lives outside
///   the VM. The `state` is ignored; the bridge is fire-and-forget
///   over an out-of-band channel (e.g. a `mpsc::Sender<String>` to
///   a WebSocket connection — see `lex-runtime::ws::serve_ws_fn_actor`).
///   `conc.ask` against a native actor returns whatever the bridge
///   produces; `conc.tell` discards it. v1 is only used internally by
///   the WS server's outbound-bridge registration; not exposed via the
///   `conc` builtin surface.
#[derive(Clone)]
pub enum ActorHandler {
    Lex(Value),
    Native(Arc<NativeActorHandler>),
}

/// Erased Rust-side handler for `ActorHandler::Native`. Boxed so we
/// can store any closure that captures (e.g. an `mpsc::Sender`).
/// Wrapped in `Arc` so cloning an `ActorCell` (which the existing
/// `conc.tell` flow does — `let handler = guard.handler.clone()`)
/// is cheap and the closure isn't duplicated.
pub struct NativeActorHandler {
    pub send: Box<dyn Fn(Value) -> Result<Value, String> + Send + Sync>,
}

impl std::fmt::Debug for NativeActorHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<native actor handler>")
    }
}

impl std::fmt::Debug for ActorHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActorHandler::Lex(v) => f.debug_tuple("Lex").field(v).finish(),
            ActorHandler::Native(n) => f.debug_tuple("Native").field(n).finish(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// String value. `SmolStr` stores strings ≤ 22 bytes inline — no heap
    /// allocation for identifiers, HTTP methods, status codes, short keys, etc.
    /// Clone of a short `SmolStr` is a 24-byte stack copy (#389 slice 4).
    Str(SmolStr),
    Bytes(Vec<u8>),
    Unit,
    List(VecDeque<Value>),
    Tuple(Vec<Value>),
    /// Record literal. `shape_id` is the `Program::record_shapes`
    /// index of the field-name vec the record was built from
    /// (#462 slice 2), so the `Op::GetField` polymorphic IC can
    /// match on a single u32 compare instead of walking the
    /// `IndexMap` by name. Records constructed outside the bytecode
    /// (JSON decode, SQL row → record, HTTP request mutators, test
    /// fixtures) have no compile-time shape and carry `NO_SHAPE_ID`
    /// — the IC unconditionally misses on them and falls through to
    /// the existing name walk.
    ///
    /// `fields` is `Box<IndexMap>` rather than `IndexMap` inline
    /// because the bare `IndexMap` is ~56B; inlining it plus
    /// `shape_id` would push `Value`'s enum size from 64B → 72B,
    /// which measurably regresses the VM stack push/pop loop
    /// (`Value` is cloned/moved on every push/pop). Boxing keeps
    /// `Value::Record` at 16B and `Value` at the pre-#462 64B.
    /// The indirection on every `IndexMap` access costs a few ns
    /// but the IC drops the field-name string compare on every
    /// hit, which is the net win on `mono_chain`.
    ///
    /// `shape_id` is **not** part of structural equality (see
    /// `PartialEq` below): two records with identical fields must
    /// compare equal regardless of provenance, so a JSON-decoded
    /// record equals a compile-time-built one with the same fields.
    Record { shape_id: u32, fields: Box<IndexMap<SmolStr, Value>> },
    /// Frame-local record (#464 step 2). Emitted by
    /// `Op::AllocStackRecord` at sites the escape analysis proved
    /// can't outlive the current call frame. `slab_start` indexes
    /// into `Vm::stack_record_arena`; the `field_count` consecutive
    /// values starting there are the record's fields, in
    /// `Program.record_shapes[shape_id]` order (same insertion order
    /// as `Op::MakeRecord` uses, so the polymorphic-IC offset is
    /// interoperable with `Value::Record`).
    ///
    /// `Op::GetField` is the only consumer that knows how to read
    /// these — every other observation point (`Op::Return`,
    /// `Op::Call`, `Op::MakeRecord` as a field value, …) is an
    /// escape op that the analysis prevents this variant from
    /// reaching. If a `StackRecord` ever does reach an unexpected
    /// site (escape-analysis bug), it surfaces as a panic at the
    /// boundary, not undefined behavior — the arena is plain
    /// `Vec<Value>` in safe Rust.
    ///
    /// Size: 4 (shape_id) + 4 (slab_start) + 2 (field_count) = 10
    /// bytes payload + tag, comfortably inside the 64B `Value`
    /// envelope.
    StackRecord { shape_id: u32, slab_start: u32, field_count: u16 },
    Variant { name: String, args: Vec<Value> },
    /// First-class function value (a lambda + its captured locals). The
    /// function's first `captures.len()` params bind to `captures`; the
    /// remaining params are supplied at call time.
    ///
    /// `fn_id` is a dense compile-time index into `Program::functions`
    /// for fast dispatch; `body_hash` is the **canonical identity** —
    /// two closures with identical bytecode bodies compare equal even
    /// when their `fn_id`s differ (which they will, when the source
    /// has the same closure literal at two locations). See `PartialEq`
    /// below and #222 for the rationale.
    Closure { fn_id: u32, body_hash: BodyHash, captures: Vec<Value> },
    /// Dense row-major `f64` matrix. A "fast lane" representation that
    /// avoids the per-element `Value::Float` boxing of `Value::List`.
    /// Used by Core's native tensor ops (matmul, dot, …) so end-to-end
    /// matmul perf hits the §13.7 #1 100ms target without paying for
    /// 2M Value boxings at the call boundary.
    F64Array { rows: u32, cols: u32, data: Vec<f64> },
    /// Persistent map keyed by `MapKey` (`Str` or `Int`). Insertion-
    /// independent equality (sorted by `BTreeMap`'s `Ord`), so two
    /// maps built from the same pairs in different orders compare
    /// equal. Restricting keys to two primitive variants keeps
    /// `Eq + Hash` requirements off `Value` itself, which has
    /// closures and floats and can't be hashed soundly.
    Map(BTreeMap<MapKey, Value>),
    /// Persistent set with the same key-type discipline as `Map`.
    Set(BTreeSet<MapKey>),
    /// Double-ended queue. O(1) push/pop on both ends; otherwise
    /// behaves like `List` for iteration / equality / JSON shape.
    /// Lex's type system tracks `Deque[T]` separately from `List[T]`
    /// so users explicitly opt in to deque semantics; the runtime
    /// uses this dedicated variant rather than backing a deque on top
    /// of `Value::List` (which would make `push_front` O(n)).
    Deque(VecDeque<Value>),
    /// A handle to a `conc.Actor`. The `Arc<Mutex<ActorCell>>` allows
    /// cheap cloning and safe concurrent access — the mutex serialises
    /// message delivery so the actor processes one message at a time.
    /// Two actor handles compare equal iff they point to the same cell
    /// (identity equality, not structural equality).
    Actor(Arc<Mutex<ActorCell>>),
    /// A periodic-tick handle returned by `conc.every` (#445). The
    /// `AtomicBool` is the cancel flag — `conc.cancel(t)` sets it and
    /// the background scheduler thread observes it on its next iteration
    /// and exits. Two ticker handles compare equal iff they point to the
    /// same cancel flag.
    Ticker(Arc<AtomicBool>),
    /// Apache Arrow `RecordBatch` — an unboxed columnar table. The
    /// "fast lane" representation for `lex-frame` and any future
    /// dataframe code: a `Value::ArrowTable` with one int64 column
    /// of N rows is N×8 bytes of contiguous memory, not N
    /// `Value::Int(_)` enum tags inside a `VecDeque`. Reductions
    /// (`arrow.col_sum_int`, `arrow.col_mean`, …) execute as one
    /// Rust call over the flat buffer, bypassing the bytecode VM
    /// for the inner loop.
    ///
    /// `Arc` makes clone cheap (refcount bump) — Arrow tables are
    /// already immutable so structural sharing across closures is
    /// safe. Equality is structural over schema + columns.
    ArrowTable(Arc<RecordBatch>),
}

/// Manual `PartialEq` for `Value` (#222). Mirrors the auto-derived
/// implementation for every variant *except* `Closure`, which compares
/// on `(body_hash, captures)` only — `fn_id` is a dense compile-time
/// index that is not stable across source-location-equivalent closure
/// literals, and including it would defeat the canonicality property
/// the `body_hash` field exists to provide.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        use Value::*;
        match (self, other) {
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => a == b,
            (Bool(a), Bool(b)) => a == b,
            (Str(a), Str(b)) => a == b,
            (Bytes(a), Bytes(b)) => a == b,
            (Unit, Unit) => true,
            (List(a), List(b)) => a == b,
            (Tuple(a), Tuple(b)) => a == b,
            (Record { fields: a, .. }, Record { fields: b, .. }) => a == b,
            // #464 step 2: a `Value::StackRecord` can only reach
            // generic equality if it crossed an escape boundary the
            // analysis was supposed to reject. Treat as a soundness
            // bug: panic rather than silently lie about equality (a
            // wrong answer would cascade into mis-routed match arms).
            // Well-typed Lex source never compares records with
            // `==` via `bin_eq` — record equality, if added, will
            // get its own opcode with arena-aware comparison.
            (StackRecord { .. }, _) | (_, StackRecord { .. }) =>
                panic!("BUG(#464): Value::StackRecord reached generic equality \
                        — escape analysis should have flagged its allocation site"),
            (Variant { name: an, args: aa }, Variant { name: bn, args: ba }) =>
                an == bn && aa == ba,
            (Closure { body_hash: ah, captures: ac, .. },
             Closure { body_hash: bh, captures: bc, .. }) =>
                ah == bh && ac == bc,
            (F64Array { rows: ar, cols: ac, data: ad },
             F64Array { rows: br, cols: bc, data: bd }) =>
                ar == br && ac == bc && ad == bd,
            (Map(a), Map(b)) => a == b,
            (Set(a), Set(b)) => a == b,
            (Deque(a), Deque(b)) => a == b,
            // Actor identity: same if both handles point to the same cell.
            (Actor(a), Actor(b)) => Arc::ptr_eq(a, b),
            // Ticker identity: same if both handles point to the same
            // cancel flag (one ticker spawn → one flag).
            (Ticker(a), Ticker(b)) => Arc::ptr_eq(a, b),
            // Arrow table equality: structural over schema + columns.
            // RecordBatch implements PartialEq directly.
            (ArrowTable(a), ArrowTable(b)) => a == b,
            _ => false,
        }
    }
}

/// Hashable, ordered key for `Value::Map` / `Value::Set`. v1
/// supports `Str` and `Int`; extending to other primitives or to
/// records is forward-compatible since the type is not exposed
/// to user code beyond the surface API.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MapKey {
    Str(String),
    Int(i64),
}

impl MapKey {
    pub fn from_value(v: &Value) -> Result<Self, String> {
        match v {
            Value::Str(s) => Ok(MapKey::Str(s.to_string())),
            Value::Int(n) => Ok(MapKey::Int(*n)),
            other => Err(format!(
                "map/set key must be Str or Int, got {other:?}")),
        }
    }
    pub fn into_value(self) -> Value {
        match self {
            MapKey::Str(s) => Value::Str(s.into()),
            MapKey::Int(n) => Value::Int(n),
        }
    }
    pub fn as_value(&self) -> Value {
        match self {
            MapKey::Str(s) => Value::Str(s.as_str().into()),
            MapKey::Int(n) => Value::Int(*n),
        }
    }
}

impl Value {
    pub fn as_int(&self) -> i64 {
        match self { Value::Int(n) => *n, other => panic!("expected Int, got {other:?}") }
    }
    pub fn as_float(&self) -> f64 {
        match self { Value::Float(n) => *n, other => panic!("expected Float, got {other:?}") }
    }
    pub fn as_bool(&self) -> bool {
        match self { Value::Bool(b) => *b, other => panic!("expected Bool, got {other:?}") }
    }
    pub fn as_str(&self) -> &str {
        match self { Value::Str(s) => s, other => panic!("expected Str, got {other:?}") }
    }

    /// Render this `Value` as a `serde_json::Value` for emission to
    /// CLI output, the agent API, conformance harness reports, etc.
    /// Canonical mapping shared across crates; previously every
    /// boundary had its own copy.
    ///
    /// Encoding:
    /// - `Variant { name, args }` → `{"$variant": name, "args": [...]}`
    /// - `F64Array { ... }` → `{"$f64_array": true, rows, cols, data}`
    /// - `Closure { body_hash, .. }` → `"<closure HEX8>"` (first 8 hex
    ///   chars of the body hash; equivalent closures across source
    ///   locations render identically — see #222)
    /// - `Bytes` → `{"$bytes": "deadbeef"}` (lowercase hex). Round-trips
    ///   through `from_json`. Bare hex strings decode as `Str`, so the
    ///   marker is required to disambiguate bytes from a string that
    ///   happens to look like hex.
    /// - `Map` with all-`Str` keys → JSON object; otherwise array of
    ///   `[key, value]` pairs (Int keys can't be JSON-object keys)
    /// - `Set` → JSON array of elements
    /// - other variants → their natural JSON shape
    ///
    /// Note: this form is **not** round-trippable for traces (see
    /// `lex-trace`'s recorder, which uses a richer marker form).
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Int(n) => J::from(*n),
            Value::Float(f) => J::from(*f),
            Value::Bool(b) => J::Bool(*b),
            Value::Str(s) => J::String(s.to_string()),
            Value::Bytes(b) => {
                let hex: String = b.iter().map(|b| format!("{:02x}", b)).collect();
                let mut m = serde_json::Map::new();
                m.insert("$bytes".into(), J::String(hex));
                J::Object(m)
            }
            Value::Unit => J::Null,
            Value::List(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Tuple(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Record { fields, .. } => {
                let mut m = serde_json::Map::new();
                for (k, v) in fields.iter() { m.insert(k.to_string(), v.to_json()); }
                J::Object(m)
            }
            // #464: should never reach JSON serialization. See PartialEq.
            Value::StackRecord { .. } =>
                panic!("BUG(#464): Value::StackRecord reached to_json — \
                        escape analysis should have prevented escape to a host boundary"),
            Value::Variant { name, args } => {
                let mut m = serde_json::Map::new();
                m.insert("$variant".into(), J::String(name.clone()));
                m.insert("args".into(), J::Array(args.iter().map(Value::to_json).collect()));
                J::Object(m)
            }
            Value::Closure { body_hash, .. } => {
                // Render the first 4 bytes (8 hex chars) of the body
                // hash. Trace stability follows: equivalent closures
                // produced from different source locations get the
                // same string. See #222.
                let prefix: String = body_hash.iter().take(4)
                    .map(|b| format!("{b:02x}")).collect();
                J::String(format!("<closure {prefix}>"))
            }
            Value::F64Array { rows, cols, data } => {
                let mut m = serde_json::Map::new();
                m.insert("$f64_array".into(), J::Bool(true));
                m.insert("rows".into(), J::from(*rows));
                m.insert("cols".into(), J::from(*cols));
                m.insert("data".into(), J::Array(data.iter().map(|f| J::from(*f)).collect()));
                J::Object(m)
            }
            Value::Map(m) => {
                let all_str = m.keys().all(|k| matches!(k, MapKey::Str(_)));
                if all_str {
                    let mut out = serde_json::Map::new();
                    for (k, v) in m {
                        if let MapKey::Str(s) = k {
                            out.insert(s.clone(), v.to_json());
                        }
                    }
                    J::Object(out)
                } else {
                    J::Array(m.iter().map(|(k, v)| {
                        J::Array(vec![k.as_value().to_json(), v.to_json()])
                    }).collect())
                }
            }
            Value::Set(s) => J::Array(
                s.iter().map(|k| k.as_value().to_json()).collect()),
            Value::Deque(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Actor(_) => J::String("<actor>".into()),
            Value::Ticker(_) => J::String("<ticker>".into()),
            Value::ArrowTable(t) => {
                // Compact summary: schema + nrows. Full data is intentionally
                // not emitted — Arrow tables can be GB-scale and a JSON dump
                // would defeat the point. Callers that need the rows go
                // through `arrow.row_at` / `arrow.col_to_*_list`.
                let mut m = serde_json::Map::new();
                m.insert("$arrow_table".into(), J::Bool(true));
                m.insert("nrows".into(), J::from(t.num_rows() as i64));
                m.insert("ncols".into(), J::from(t.num_columns() as i64));
                let cols: Vec<J> = t
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| {
                        let mut o = serde_json::Map::new();
                        o.insert("name".into(), J::String(f.name().clone()));
                        o.insert("type".into(), J::String(format!("{}", f.data_type())));
                        J::Object(o)
                    })
                    .collect();
                m.insert("schema".into(), J::Array(cols));
                J::Object(m)
            }
        }
    }

    /// Decode a `serde_json::Value` into a `Value`. The inverse of
    /// [`to_json`](Self::to_json) for the shapes Lex round-trips:
    ///
    /// - `{"$variant": "Name", "args": [...]}` → `Value::Variant`
    /// - `{"$bytes": "deadbeef"}` → `Value::Bytes` (lowercase hex; an
    ///   odd-length string or non-hex character falls through to
    ///   `Value::Record`, matching the malformed-`$variant` fallback)
    /// - JSON object → `Value::Record`
    /// - JSON array → `Value::List`
    /// - JSON null → `Value::Unit`
    /// - JSON string / bool / number → the corresponding scalar
    ///
    /// Map, Set, F64Array, and Closure don't round-trip — they decode
    /// as their natural JSON shape (Object / Array / Object / Str
    /// respectively), since the CLI / HTTP / VM callers building Values
    /// from JSON don't have those shapes in their input vocabulary.
    pub fn from_json(v: &serde_json::Value) -> Value {
        use serde_json::Value as J;
        match v {
            J::Null => Value::Unit,
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => {
                if let Some(i) = n.as_i64() { Value::Int(i) }
                else if let Some(f) = n.as_f64() { Value::Float(f) }
                else { Value::Unit }
            }
            J::String(s) => Value::Str(s.as_str().into()),
            J::Array(items) => Value::List(items.iter().map(Value::from_json).collect::<VecDeque<_>>()),
            J::Object(map) => {
                if let (Some(J::String(name)), Some(J::Array(args))) =
                    (map.get("$variant"), map.get("args"))
                {
                    return Value::Variant {
                        name: name.clone(),
                        args: args.iter().map(Value::from_json).collect(),
                    };
                }
                if map.len() == 1 {
                    if let Some(J::String(hex)) = map.get("$bytes") {
                        if let Some(bytes) = decode_hex(hex) {
                            return Value::Bytes(bytes);
                        }
                    }
                }
                let mut out = indexmap::IndexMap::new();
                for (k, v) in map {
                    out.insert(k.clone(), Value::from_json(v));
                }
                Value::record_dynamic(out)
            }
        }
    }

    /// Build a `Value::Record` whose fields don't come from an
    /// `Op::MakeRecord` site — JSON decode, SQL row → record, host
    /// effect handlers, test fixtures, etc. Interns the field-name
    /// set in the process-global shape registry (#462 slice 3) so
    /// records with the same set of field names share a stable
    /// `shape_id` and hit the same IC slot. Two records with the
    /// same fields in different insertion order share a `shape_id`
    /// (the registry sorts the field-name vec before lookup),
    /// matching the existing `Value::Record` structural-equality
    /// semantics.
    ///
    /// Dynamic shape IDs live in the high half of the `u32` range
    /// (see `crate::shape_registry::DYNAMIC_SHAPE_ID_BASE`) so they
    /// can't collide with the per-program shape indices emitted by
    /// `Op::MakeRecord`. Mixed-flavor IC sites (which the slice-2b
    /// measurement found at exactly zero occurrences) would still
    /// be correct under the IC's shape-keyed verifier — they'd just
    /// churn the cache.
    /// Build a `Record` from a String-keyed host map (JSON decode, SQL
    /// rows, builtins). Keys are re-collected into interned `SmolStr`
    /// (#461 field-name interning). The hot bytecode `MakeRecord` path
    /// builds `SmolStr`-keyed maps directly and never routes through
    /// here; callers that already hold an interned map use
    /// `record_interned`.
    pub fn record_dynamic(fields: IndexMap<String, Value>) -> Value {
        let shape_id = crate::shape_registry::intern(fields.keys());
        let fields: IndexMap<SmolStr, Value> =
            fields.into_iter().map(|(k, v)| (SmolStr::from(k), v)).collect();
        Value::Record { shape_id, fields: Box::new(fields) }
    }

    /// Build a `Record` from an already-interned `SmolStr`-keyed map —
    /// used by the http builder chain, which threads `SmolStr` keys
    /// through `with_header`/`with_query`/… without round-tripping back
    /// to `String` (#461).
    pub fn record_interned(fields: IndexMap<SmolStr, Value>) -> Value {
        let shape_id = crate::shape_registry::intern(fields.keys());
        Value::Record { shape_id, fields: Box::new(fields) }
    }
}

/// Sentinel `shape_id` for records constructed outside an
/// `Op::MakeRecord` site (#462 slice 2). `Program::record_shapes`
/// is bounded by `u32::MAX - 1` in practice (each compile-time
/// record literal adds one entry), so reserving the top of the
/// `u32` range as "no shape" keeps `Value::Record.shape_id` a flat
/// `u32` — the `Op::GetField` IC's hot path is a single u32
/// compare, no `Option` discriminant.
pub const NO_SHAPE_ID: u32 = u32::MAX;

/// Lowercase-hex → bytes. Returns `None` for odd length or non-hex chars
/// (callers fall through to a record decode rather than erroring).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) { return None; }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}
