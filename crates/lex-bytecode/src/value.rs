//! Runtime values.

use indexmap::IndexMap;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    Unit,
    List(Vec<Value>),
    Tuple(Vec<Value>),
    Record(IndexMap<String, Value>),
    Variant { name: String, args: Vec<Value> },
    /// First-class function value (a lambda + its captured locals). The
    /// function's first `captures.len()` params bind to `captures`; the
    /// remaining params are supplied at call time.
    Closure { fn_id: u32, captures: Vec<Value> },
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
            Value::Str(s) => Ok(MapKey::Str(s.clone())),
            Value::Int(n) => Ok(MapKey::Int(*n)),
            other => Err(format!(
                "map/set key must be Str or Int, got {other:?}")),
        }
    }
    pub fn into_value(self) -> Value {
        match self {
            MapKey::Str(s) => Value::Str(s),
            MapKey::Int(n) => Value::Int(n),
        }
    }
    pub fn as_value(&self) -> Value {
        match self {
            MapKey::Str(s) => Value::Str(s.clone()),
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
    /// - `Closure { fn_id, .. }` → `"<closure fn_N>"`
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
            Value::Str(s) => J::String(s.clone()),
            Value::Bytes(b) => {
                let hex: String = b.iter().map(|b| format!("{:02x}", b)).collect();
                let mut m = serde_json::Map::new();
                m.insert("$bytes".into(), J::String(hex));
                J::Object(m)
            }
            Value::Unit => J::Null,
            Value::List(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Tuple(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Record(fields) => {
                let mut m = serde_json::Map::new();
                for (k, v) in fields { m.insert(k.clone(), v.to_json()); }
                J::Object(m)
            }
            Value::Variant { name, args } => {
                let mut m = serde_json::Map::new();
                m.insert("$variant".into(), J::String(name.clone()));
                m.insert("args".into(), J::Array(args.iter().map(Value::to_json).collect()));
                J::Object(m)
            }
            Value::Closure { fn_id, .. } => J::String(format!("<closure fn_{fn_id}>")),
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
            J::String(s) => Value::Str(s.clone()),
            J::Array(items) => Value::List(items.iter().map(Value::from_json).collect()),
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
                Value::Record(out)
            }
        }
    }
}

/// Lowercase-hex → bytes. Returns `None` for odd length or non-hex chars
/// (callers fall through to a record decode rather than erroring).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}
