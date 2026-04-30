//! Runtime values.

use indexmap::IndexMap;

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
