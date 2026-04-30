//! Canonical JSON encoding (RFC 8785-flavored).
//!
//! We emit:
//! - UTF-8 bytes
//! - No whitespace
//! - Object keys sorted (lexicographic, byte-wise on UTF-8)
//! - Numbers as their `serde_json` default rendering
//! - Strings escaped per JSON spec
//!
//! Two values that serde-serialize to equal `serde_json::Value` must produce
//! byte-identical output.

use serde_json::Value;

pub fn to_canonical_string(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

pub fn hash_canonical(value: &Value) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let s = to_canonical_string(value);
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let r = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&r);
    out
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 { out.push(','); }
                write_value(it, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 { out.push(','); }
                write_string(k, out);
                out.push(':');
                write_value(&map[*k], out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
