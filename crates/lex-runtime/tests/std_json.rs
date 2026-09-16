//! `std.json.decode` / `encode` / `encode_pretty` — the native generic
//! `Json` value ADT path (#885). `decode` is a total parse into
//! JNull/JBool/JInt/JFloat/JStr/JList/JObj; `encode` is its inverse.
//! Native (serde_json), O(n) — the drop-in for lex-schema's interpreted
//! `json_value`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

/// Assert `v` is `Variant(tag, ..)` and return a clone of its payload args.
fn payload(v: &Value, tag: &str) -> Vec<Value> {
    match v {
        Value::Variant { name, args } if name == tag => args.clone(),
        other => panic!("expected {tag}, got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.json" as json

fn dec(s :: Str) -> Result[Json, Str] { json.decode(s) }
fn enc(j :: Json) -> Str { json.encode(j) }
fn enc_pretty(j :: Json, n :: Int) -> Str { json.encode_pretty(j, n) }

# round-trip a source string through decode -> encode
fn roundtrip(s :: Str) -> Result[Str, Str] {
  match json.decode(s) {
    Ok(j) => Ok(json.encode(j)),
    Err(e) => Err(e),
  }
}
"#;

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn dec_ok(src_json: &str) -> Value {
    let r = run(SRC, "dec", vec![s(src_json)]);
    payload(&r, "Ok").remove(0)
}

#[test]
fn decode_scalars_build_the_right_variants() {
    assert_eq!(payload(&dec_ok("\"hi\""), "JStr")[0], Value::Str("hi".into()));
    assert_eq!(payload(&dec_ok("42"), "JInt")[0], Value::Int(42));
    assert_eq!(payload(&dec_ok("3.5"), "JFloat")[0], Value::Float(3.5));
    assert_eq!(payload(&dec_ok("true"), "JBool")[0], Value::Bool(true));
    assert!(payload(&dec_ok("null"), "JNull").is_empty());
}

#[test]
fn int_vs_float_distinction_preserved() {
    // whole number with no dot is JInt; 7.0 is JFloat.
    payload(&dec_ok("7"), "JInt");
    payload(&dec_ok("7.0"), "JFloat");
}

#[test]
fn decode_object_and_array_are_jobj_jlist() {
    let jobj = dec_ok(r#"{"a":[1,2],"b":"x"}"#);
    let entries = match &payload(&jobj, "JObj")[0] {
        Value::List(l) => l.clone(),
        o => panic!("JObj payload not a list: {o:?}"),
    };
    assert_eq!(entries.len(), 2);
    // first entry: ("a", JList([JInt(1), JInt(2)]))
    match entries.iter().next() {
        Some(Value::Tuple(kv)) => {
            assert_eq!(kv[0], Value::Str("a".into()));
            let items = match &payload(&kv[1], "JList")[0] {
                Value::List(l) => l.clone(),
                o => panic!("not a list: {o:?}"),
            };
            assert_eq!(items.len(), 2);
            assert_eq!(payload(items.iter().next().unwrap(), "JInt")[0], Value::Int(1));
        }
        o => panic!("first entry not a pair: {o:?}"),
    }
}

#[test]
fn decode_error_is_err_not_panic() {
    let v = run(SRC, "dec", vec![s("{not json")]);
    match v {
        Value::Variant { name, .. } if name == "Err" => {}
        other => panic!("expected Err on malformed input, got {other:?}"),
    }
}

#[test]
fn roundtrip_preserves_structure_and_values() {
    let cases = [
        "\"hi\"",
        "42",
        "true",
        "null",
        "[1,2,3]",
        r#"{"a":1,"b":[true,null,"x"]}"#,
        r#"{"nested":{"deep":{"n":10}}}"#,
    ];
    for c in cases {
        let rt = run(SRC, "roundtrip", vec![s(c)]);
        let out = payload(&rt, "Ok").remove(0);
        let out_str = match out {
            Value::Str(ref t) => t.to_string(),
            o => panic!("roundtrip not a Str: {o:?}"),
        };
        // Compare as decoded Json so key-order/whitespace don't matter.
        let a = run(SRC, "dec", vec![s(c)]);
        let b = run(SRC, "dec", vec![s(&out_str)]);
        assert_eq!(a, b, "roundtrip changed the value for {c}");
    }
}

#[test]
fn encode_from_hand_built_json() {
    // JObj([("k", JInt(1))]) encodes to {"k":1}
    let src = r#"
    import "std.json" as json
    fn go() -> Str { json.encode(JObj([("k", JInt(1))])) }
    "#;
    assert_eq!(run(src, "go", vec![]), Value::Str("{\"k\":1}".into()));
}

#[test]
fn encode_pretty_indents() {
    let src = r#"
    import "std.json" as json
    fn go() -> Str { json.encode_pretty(JObj([("k", JInt(1))]), 2) }
    "#;
    assert_eq!(run(src, "go", vec![]), Value::Str("{\n  \"k\": 1\n}".into()));
}

#[test]
fn user_defined_json_type_still_compiles() {
    // The builtin `Json` is registered globally, but lex-schema's
    // `json_value` (imported across the ecosystem) declares its OWN
    // `type Json` with the same constructor names. A module that defines
    // its own `Json` must keep compiling — its declaration shadows the
    // builtin within that module. (Verified end-to-end: lex-schema's full
    // test suite passes on this toolchain.)
    let src = r#"
    type Json = JNull | JBool(Bool) | JInt(Int) | JStr(Str) | JList(List[Json])
    fn wrap(s :: Str) -> Json { JStr(s) }
    fn unwrap(j :: Json) -> Str { match j { JStr(s) => s, _ => "?" } }
    fn go() -> Str { unwrap(wrap("hi")) }
    "#;
    assert_eq!(run(src, "go", vec![]), Value::Str("hi".into()));
}

#[test]
fn empty_containers_and_pure_effects() {
    // Empty object/array round-trip; all ran under Policy::pure(), proving
    // decode/encode need no effect row.
    assert_eq!(
        run(SRC, "roundtrip", vec![s("{}")]),
        Value::Variant { name: "Ok".into(), args: vec![Value::Str("{}".into())] }
    );
    assert_eq!(
        run(SRC, "roundtrip", vec![s("[]")]),
        Value::Variant { name: "Ok".into(), args: vec![Value::Str("[]".into())] }
    );
}
