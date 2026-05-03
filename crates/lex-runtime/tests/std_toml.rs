//! Integration tests for `std.toml`. The first slice of #98's
//! follow-up `std.config` umbrella — TOML parsing and serialization
//! routed through the same `Value` shape as `std.json` so the two
//! compose.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::pure());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" => {
            args.into_iter().next().expect("Ok payload")
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

fn err_msg(v: Value) -> String {
    match v {
        Value::Variant { name, args } if name == "Err" => match args.into_iter().next() {
            Some(Value::Str(s)) => s,
            other => panic!("expected Err(Str), got {other:?}"),
        },
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn parse_simple_table_into_record() {
    let src = r#"
import "std.toml" as toml

fn parse(s :: Str) -> Result[{ name :: Str, version :: Str }, Str] {
  toml.parse(s)
}
"#;
    let toml_src = r#"
name = "lex"
version = "0.1.0"
"#;
    let v = run(src, "parse", vec![Value::Str(toml_src.into())]);
    let r = match unwrap_ok(v) {
        Value::Record(r) => r,
        other => panic!("expected Record, got {other:?}"),
    };
    assert_eq!(r.get("name"), Some(&Value::Str("lex".into())));
    assert_eq!(r.get("version"), Some(&Value::Str("0.1.0".into())));
}

#[test]
fn parse_nested_table() {
    let src = r#"
import "std.toml" as toml

# Mirrors the shape of a Cargo.toml [package] section.
fn parse(s :: Str) -> Result[{
  package :: { name :: Str, version :: Str, edition :: Str }
}, Str] {
  toml.parse(s)
}
"#;
    let toml_src = r#"
[package]
name = "demo"
version = "1.2.3"
edition = "2024"
"#;
    let v = run(src, "parse", vec![Value::Str(toml_src.into())]);
    let outer = match unwrap_ok(v) {
        Value::Record(r) => r,
        other => panic!("expected outer Record, got {other:?}"),
    };
    let pkg = match outer.get("package") {
        Some(Value::Record(r)) => r,
        other => panic!("expected nested Record, got {other:?}"),
    };
    assert_eq!(pkg.get("name"), Some(&Value::Str("demo".into())));
    assert_eq!(pkg.get("version"), Some(&Value::Str("1.2.3".into())));
    assert_eq!(pkg.get("edition"), Some(&Value::Str("2024".into())));
}

#[test]
fn parse_array_of_tables() {
    let src = r#"
import "std.toml" as toml
import "std.list" as list

fn dep_count(s :: Str) -> Int {
  let parsed :: Result[{ dependencies :: List[{ name :: Str, version :: Str }] }, Str] := toml.parse(s)
  match parsed {
    Ok(r) => list.len(r.dependencies),
    Err(_) => 0 - 1,
  }
}
"#;
    let toml_src = r#"
[[dependencies]]
name = "serde"
version = "1.0"

[[dependencies]]
name = "tokio"
version = "1.0"

[[dependencies]]
name = "anyhow"
version = "1.0"
"#;
    let v = run(src, "dep_count", vec![Value::Str(toml_src.into())]);
    assert_eq!(v, Value::Int(3));
}

#[test]
fn parse_scalar_types_round_trip() {
    let src = r#"
import "std.toml" as toml

fn parse(s :: Str) -> Result[{
  name :: Str,
  count :: Int,
  ratio :: Float,
  enabled :: Bool,
  tags :: List[Str]
}, Str] {
  toml.parse(s)
}
"#;
    let toml_src = r#"
name = "demo"
count = 42
ratio = 1.5
enabled = true
tags = ["alpha", "beta", "gamma"]
"#;
    let v = run(src, "parse", vec![Value::Str(toml_src.into())]);
    let r = match unwrap_ok(v) {
        Value::Record(r) => r,
        other => panic!("expected Record, got {other:?}"),
    };
    assert_eq!(r.get("count"), Some(&Value::Int(42)));
    assert_eq!(r.get("ratio"), Some(&Value::Float(1.5)));
    assert_eq!(r.get("enabled"), Some(&Value::Bool(true)));
    assert_eq!(
        r.get("tags"),
        Some(&Value::List(vec![
            Value::Str("alpha".into()),
            Value::Str("beta".into()),
            Value::Str("gamma".into()),
        ])),
    );
}

#[test]
fn parse_datetime_becomes_iso_string() {
    // TOML datetimes don't have a clean Lex equivalent — we render
    // them as RFC 3339 strings so callers can pipe through
    // `datetime.parse_iso` if they need an actual Instant.
    let src = r#"
import "std.toml" as toml

fn parse(s :: Str) -> Result[{ created :: Str }, Str] {
  toml.parse(s)
}
"#;
    let toml_src = r#"created = 2026-05-03T12:00:00Z
"#;
    let v = run(src, "parse", vec![Value::Str(toml_src.into())]);
    let r = match unwrap_ok(v) {
        Value::Record(r) => r,
        other => panic!("expected Record, got {other:?}"),
    };
    let created = match r.get("created") {
        Some(Value::Str(s)) => s,
        other => panic!("expected Str, got {other:?}"),
    };
    assert!(
        created.starts_with("2026-05-03"),
        "expected ISO datetime starting 2026-05-03, got {created}",
    );
}

#[test]
fn parse_returns_err_on_malformed_input() {
    let src = r#"
import "std.toml" as toml

fn parse_ok(s :: Str) -> Bool {
  match toml.parse(s) {
    Ok(_)  => true,
    Err(_) => false,
  }
}
"#;
    // unterminated string + no key/value separator
    let v = run(src, "parse_ok", vec![Value::Str("name = \"unclosed\nbroken".into())]);
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn parse_then_stringify_round_trip() {
    let src = r#"
import "std.toml" as toml

fn round_trip(s :: Str) -> Result[Str, Str] {
  match toml.parse(s) {
    Ok(v)  => toml.stringify(v),
    Err(e) => Err(e),
  }
}
"#;
    let toml_src = r#"
name = "lex"
version = "0.1.0"
"#;
    let v = run(src, "round_trip", vec![Value::Str(toml_src.into())]);
    let serialized = match unwrap_ok(v) {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    };
    // Re-parse to confirm the round-tripped TOML is structurally
    // equivalent (TOML's serializer may reorder keys / quote
    // differently, so a byte-equal compare would be too strict).
    let toml_v: toml::Value = toml::from_str(&serialized).expect("re-parse");
    assert_eq!(toml_v["name"].as_str(), Some("lex"));
    assert_eq!(toml_v["version"].as_str(), Some("0.1.0"));
}

#[test]
fn stringify_returns_err_for_top_level_scalar() {
    // TOML's top level must be a table; stringifying a bare Int
    // surfaces as Err rather than panicking.
    let src = r#"
import "std.toml" as toml

fn stringify_int() -> Result[Str, Str] {
  toml.stringify(42)
}
"#;
    let v = run(src, "stringify_int", vec![]);
    let msg = err_msg(v);
    assert!(
        msg.contains("toml.stringify") || msg.to_lowercase().contains("table"),
        "expected toml.stringify error, got: {msg}",
    );
}

#[test]
fn parse_handles_realistic_cargo_toml_subset() {
    // A trimmed Cargo.toml — the actual workload `std.config (TOML)`
    // ships to unblock. Contains [package], [dependencies], and a
    // nested dependency object with feature flags.
    let src = r#"
import "std.toml" as toml

fn package_name(s :: Str) -> Str {
  let parsed :: Result[{ package :: { name :: Str } }, Str] := toml.parse(s)
  match parsed {
    Ok(r) => r.package.name,
    Err(_) => "<parse failed>",
  }
}
"#;
    let cargo_toml = r#"
[package]
name = "lex-runtime"
version = "0.1.0"
edition = "2024"

[dependencies]
serde = { version = "1.0", features = ["derive"] }
toml = "0.9"
indexmap = "2"
"#;
    let v = run(src, "package_name", vec![Value::Str(cargo_toml.into())]);
    assert_eq!(v, Value::Str("lex-runtime".into()));
}
