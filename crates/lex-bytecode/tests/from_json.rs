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
