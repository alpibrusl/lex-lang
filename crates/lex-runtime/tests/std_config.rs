//! Integration tests for `std.yaml` / `std.dotenv` / `std.csv`.
//!
//! These are pure parsers; no effects required. They mirror
//! `std.toml`'s shape — `parse :: Str -> Result[T, Str]` with
//! the type checker inferring `T` from the call site.

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

// ---- yaml ----

const YAML_SRC: &str = r#"
import "std.yaml" as yaml

type Cargo = { name :: Str, version :: Str }

fn parse_cargo(s :: Str) -> Result[Cargo, Str] { yaml.parse(s) }

fn round_trip(s :: Str) -> Result[Str, Str] {
  let parsed :: Result[Cargo, Str] := yaml.parse(s)
  match parsed {
    Ok(v)  => yaml.stringify(v),
    Err(e) => Err(e),
  }
}
"#;

#[test]
fn yaml_parse_typed_record() {
    let v = run(YAML_SRC, "parse_cargo", vec![Value::Str(
        "name: lex\nversion: 0.1.0\n".into()
    )]);
    match v {
        Value::Variant { name, args } if name == "Ok" => {
            match &args[0] {
                Value::Record(fields) => {
                    let mut m: std::collections::BTreeMap<&str, &Value> = fields.iter()
                        .map(|(k, v)| (k.as_str(), v)).collect();
                    assert_eq!(m.remove("name"), Some(&Value::Str("lex".into())));
                    assert_eq!(m.remove("version"), Some(&Value::Str("0.1.0".into())));
                }
                other => panic!("expected Record, got {other:?}"),
            }
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn yaml_parse_returns_err_on_garbage() {
    let v = run(YAML_SRC, "parse_cargo", vec![Value::Str("[: this is not yaml".into())]);
    match v {
        Value::Variant { name, .. } if name == "Err" => {}
        other => panic!("expected Err, got {other:?}"),
    }
}

#[test]
fn yaml_round_trip() {
    let v = run(YAML_SRC, "round_trip", vec![Value::Str("name: lex\nversion: 0.1.0\n".into())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Str(s) => {
                assert!(s.contains("name") && s.contains("lex"), "round-trip lost data: {s}");
            }
            other => panic!("expected Str, got {other:?}"),
        },
        other => panic!("expected Ok, got {other:?}"),
    }
}

// ---- dotenv ----

const DOTENV_SRC: &str = r#"
import "std.dotenv" as dotenv
import "std.map"    as map

fn parse_count(s :: Str) -> Result[Int, Str] {
  match dotenv.parse(s) {
    Ok(m)  => Ok(map.size(m)),
    Err(e) => Err(e),
  }
}

fn parse_get(s :: Str, k :: Str) -> Result[Option[Str], Str] {
  match dotenv.parse(s) {
    Ok(m)  => Ok(map.get(m, k)),
    Err(e) => Err(e),
  }
}
"#;

#[test]
fn dotenv_parses_simple_pairs() {
    let body = "DB_URL=postgres://localhost/lex\nLOG_LEVEL=info\n";
    let v = run(DOTENV_SRC, "parse_count", vec![Value::Str(body.into())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => assert_eq!(args[0], Value::Int(2)),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn dotenv_skips_blank_and_comment_lines() {
    let body = "# header\n\nKEY=value\n# trailing comment\n";
    let v = run(DOTENV_SRC, "parse_count", vec![Value::Str(body.into())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => assert_eq!(args[0], Value::Int(1)),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn dotenv_handles_quoted_values_and_export_prefix() {
    let body = "export A=\"hello world\"\nB='single'\nC=plain\n";
    let v = run(DOTENV_SRC, "parse_get", vec![
        Value::Str(body.into()), Value::Str("A".into()),
    ]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Variant { name, args } if name == "Some" => {
                assert_eq!(&args[0], &Value::Str("hello world".into()));
            }
            other => panic!("expected Some, got {other:?}"),
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn dotenv_returns_err_on_missing_equals() {
    let body = "KEY_WITHOUT_EQUALS\n";
    let v = run(DOTENV_SRC, "parse_count", vec![Value::Str(body.into())]);
    match v {
        Value::Variant { name, .. } if name == "Err" => {}
        other => panic!("expected Err, got {other:?}"),
    }
}

// ---- csv ----

const CSV_SRC: &str = r#"
import "std.csv"  as csv
import "std.list" as list

fn row_count(s :: Str) -> Result[Int, Str] {
  match csv.parse(s) {
    Ok(rows) => Ok(list.len(rows)),
    Err(e)   => Err(e),
  }
}

fn round_trip(rows :: List[List[Str]]) -> Result[Str, Str] { csv.stringify(rows) }
"#;

#[test]
fn csv_parse_counts_rows() {
    let body = "name,version\nlex,0.1.0\nrust,1.94\n";
    let v = run(CSV_SRC, "row_count", vec![Value::Str(body.into())]);
    match v {
        Value::Variant { name, args } if name == "Ok" => assert_eq!(args[0], Value::Int(3)),
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn csv_stringify_round_trips() {
    let rows = Value::List(vec![
        Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
        Value::List(vec![Value::Str("1".into()), Value::Str("2".into())]),
    ]);
    let v = run(CSV_SRC, "round_trip", vec![rows]);
    match v {
        Value::Variant { name, args } if name == "Ok" => match &args[0] {
            Value::Str(s) => {
                assert!(s.contains("a,b"), "got: {s:?}");
                assert!(s.contains("1,2"), "got: {s:?}");
            }
            other => panic!("expected Str, got {other:?}"),
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}
