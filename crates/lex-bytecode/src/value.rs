//! Runtime values.

use indexmap::IndexMap;
use std::collections::{BTreeMap, BTreeSet};

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
}
