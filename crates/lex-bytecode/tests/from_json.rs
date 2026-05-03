//! `Value::from_json` round-trip and variant-detection tests.
//! Closes #93's symmetry concern: every JSON shape `to_json` produces
//! decodes back to a structurally-equal `Value`.

use lex_bytecode::Value;
use serde_json::json;

fn roundtrip(v: Value) {
    let j = v.to_json();
    let back = Value::from_json(&j);
    assert_eq!(v, back, "value did not round-trip; intermediate JSON: {j}");
}

#[test]
fn variant_no_args_decodes_as_variant() {
    let j = json!({ "$variant": "Red", "args": [] });
    let v = Value::from_json(&j);
    assert_eq!(
        v,
        Value::Variant {
            name: "Red".into(),
            args: vec![]
        }
    );
}

#[test]
fn variant_with_args_decodes_as_variant() {
    let j = json!({ "$variant": "Some", "args": [42] });
    let v = Value::from_json(&j);
    assert_eq!(
        v,
        Value::Variant {
            name: "Some".into(),
            args: vec![Value::Int(42)]
        }
    );
}

#[test]
fn variant_with_nested_variant_arg_decodes_recursively() {
    let j = json!({
        "$variant": "Ok",
        "args": [{ "$variant": "Some", "args": [7] }]
    });
    let v = Value::from_json(&j);
    assert_eq!(
        v,
        Value::Variant {
            name: "Ok".into(),
            args: vec![Value::Variant {
                name: "Some".into(),
                args: vec![Value::Int(7)]
            }]
        }
    );
}

#[test]
fn plain_object_without_variant_keys_stays_record() {
    let j = json!({ "x": 1, "y": 2 });
    let v = Value::from_json(&j);
    let expected = {
        let mut m = indexmap::IndexMap::new();
        m.insert("x".into(), Value::Int(1));
        m.insert("y".into(), Value::Int(2));
        Value::Record(m)
    };
    assert_eq!(v, expected);
}

#[test]
fn object_with_only_variant_key_stays_record() {
    // Both `$variant` AND `args` are required for the variant decode.
    let j = json!({ "$variant": "Red" });
    let v = Value::from_json(&j);
    let expected = {
        let mut m = indexmap::IndexMap::new();
        m.insert("$variant".into(), Value::Str("Red".into()));
        Value::Record(m)
    };
    assert_eq!(v, expected);
}

#[test]
fn variant_round_trip() {
    roundtrip(Value::Variant {
        name: "Healthy".into(),
        args: vec![],
    });
    roundtrip(Value::Variant {
        name: "Some".into(),
        args: vec![Value::Int(42)],
    });
    roundtrip(Value::Variant {
        name: "Pair".into(),
        args: vec![Value::Str("a".into()), Value::Bool(true)],
    });
}

#[test]
fn record_round_trip() {
    let mut m = indexmap::IndexMap::new();
    m.insert("name".into(), Value::Str("alice".into()));
    m.insert("age".into(), Value::Int(30));
    roundtrip(Value::Record(m));
}

#[test]
fn nested_variant_in_record_round_trip() {
    let mut m = indexmap::IndexMap::new();
    m.insert(
        "result".into(),
        Value::Variant {
            name: "Ok".into(),
            args: vec![Value::Int(7)],
        },
    );
    roundtrip(Value::Record(m));
}

#[test]
fn list_round_trip() {
    roundtrip(Value::List(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
    ]));
}

#[test]
fn list_of_variants_round_trips() {
    roundtrip(Value::List(vec![
        Value::Variant {
            name: "Red".into(),
            args: vec![],
        },
        Value::Variant {
            name: "Green".into(),
            args: vec![],
        },
    ]));
}

#[test]
fn primitives_round_trip() {
    roundtrip(Value::Int(42));
    roundtrip(Value::Bool(true));
    roundtrip(Value::Str("hi".into()));
    roundtrip(Value::Unit);
}

#[test]
fn bytes_round_trip_via_dollar_bytes_marker() {
    // The marker disambiguates bytes from a string-that-happens-to-be-hex.
    roundtrip(Value::Bytes(vec![]));
    roundtrip(Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
    roundtrip(Value::Bytes((0..=255u8).collect()));
}

#[test]
fn bytes_encode_uses_dollar_bytes_object() {
    let j = Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]).to_json();
    assert_eq!(j, json!({ "$bytes": "deadbeef" }));
}

#[test]
fn dollar_bytes_decodes_as_bytes() {
    let j = json!({ "$bytes": "deadbeef" });
    assert_eq!(Value::from_json(&j), Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
}

#[test]
fn bare_hex_string_still_decodes_as_str() {
    // No marker → no bytes. Avoids accidentally classifying user strings
    // that happen to be valid hex.
    let j = json!("deadbeef");
    assert_eq!(Value::from_json(&j), Value::Str("deadbeef".into()));
}

#[test]
fn dollar_bytes_with_invalid_hex_falls_through_to_record() {
    // Odd length → invalid hex → record fallback (parallels the
    // malformed-`$variant` fallback above).
    let j = json!({ "$bytes": "abc" });
    let v = Value::from_json(&j);
    let expected = {
        let mut m = indexmap::IndexMap::new();
        m.insert("$bytes".into(), Value::Str("abc".into()));
        Value::Record(m)
    };
    assert_eq!(v, expected);
}

#[test]
fn dollar_bytes_with_extra_keys_stays_record() {
    // Extra keys signal it's not the marker shape.
    let j = json!({ "$bytes": "dead", "note": "x" });
    let v = Value::from_json(&j);
    let expected = {
        let mut m = indexmap::IndexMap::new();
        m.insert("$bytes".into(), Value::Str("dead".into()));
        m.insert("note".into(), Value::Str("x".into()));
        Value::Record(m)
    };
    assert_eq!(v, expected);
}
