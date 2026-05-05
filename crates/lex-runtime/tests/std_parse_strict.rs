//! `*.parse_strict` — required-field validation at parse time
//! (tactical fix for #168). The full type-driven fix — deriving
//! the required-field list from the target `T` at type-check
//! time, so plain `parse[T]` validates without the user passing
//! field names — is still tracked in #168 itself.

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

fn ok(v: &Value) -> &Value {
    match v {
        Value::Variant { name, args } if name == "Ok" => &args[0],
        other => panic!("expected Ok, got {other:?}"),
    }
}

fn err_msg(v: &Value) -> &str {
    match v {
        Value::Variant { name, args } if name == "Err" => match &args[0] {
            Value::Str(s) => s.as_str(),
            other => panic!("expected Err(Str), got {other:?}"),
        },
        other => panic!("expected Err, got {other:?}"),
    }
}

// ---- toml ---------------------------------------------------------

const TOML_SRC: &str = r#"
import "std.toml" as toml

type Manifest = { license :: Str, version :: Str }

# Plain parse — pre-#168 behavior, returns Ok with whatever's there.
fn plain(s :: Str) -> Result[Manifest, Str] { toml.parse(s) }

# Strict parse — caller declares the fields T requires; runtime
# checks the parsed table for them.
fn strict(s :: Str, required :: List[Str]) -> Result[Manifest, Str] {
  toml.parse_strict(s, required)
}
"#;

#[test]
fn toml_parse_strict_passes_when_all_fields_present() {
    let src = "license = \"EUPL-1.2\"\nversion = \"0.1.0\"\n";
    let v = run(TOML_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("license".into()), Value::Str("version".into())]),
    ]);
    let m = ok(&v);
    // Should be a Record with both fields.
    match m {
        Value::Record(fields) => {
            assert!(fields.contains_key("license"));
            assert!(fields.contains_key("version"));
        }
        other => panic!("expected Record, got {other:?}"),
    }
}

#[test]
fn toml_parse_strict_fails_when_required_field_missing() {
    // Reproduces the rubric scenario: TOML has `version` but no
    // `license`. Plain `parse` returns Ok(incomplete record);
    // `parse_strict` returns Err naming the missing field.
    let src = "version = \"0.1.0\"\n";
    let v = run(TOML_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("license".into()), Value::Str("version".into())]),
    ]);
    let detail = err_msg(&v);
    assert!(detail.contains("missing required field"), "got: {detail}");
    assert!(detail.contains("license"), "should name the missing field: {detail}");
}

#[test]
fn toml_parse_with_empty_required_degenerates_to_plain_parse() {
    let src = "version = \"0.1.0\"\n";
    let v = run(TOML_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![]),
    ]);
    // Empty required-list ⇒ no validation ⇒ behaves like plain parse.
    let _ = ok(&v);
}

#[test]
fn toml_plain_parse_still_returns_ok_on_incomplete_record() {
    // Documents the original #168 behavior: plain `parse` doesn't
    // validate. This test pins the existing semantics so the
    // tactical fix doesn't change them silently — when the full
    // type-driven fix lands in #168 it will modify this test.
    let src = "version = \"0.1.0\"\n";
    let v = run(TOML_SRC, "plain", vec![Value::Str(src.into())]);
    // Plain parse returns Ok even though `license` is missing.
    let _ = ok(&v);
}

// ---- yaml ---------------------------------------------------------

const YAML_SRC: &str = r#"
import "std.yaml" as yaml

type Cargo = { name :: Str, version :: Str }

fn strict(s :: Str, required :: List[Str]) -> Result[Cargo, Str] {
  yaml.parse_strict(s, required)
}
"#;

#[test]
fn yaml_parse_strict_fails_on_missing_field() {
    let src = "name: lex\n";
    let v = run(YAML_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("name".into()), Value::Str("version".into())]),
    ]);
    let detail = err_msg(&v);
    assert!(detail.contains("version"), "should name missing field: {detail}");
}

#[test]
fn yaml_parse_strict_passes_when_all_present() {
    let src = "name: lex\nversion: 0.1.0\n";
    let v = run(YAML_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("name".into()), Value::Str("version".into())]),
    ]);
    let _ = ok(&v);
}

// ---- json ---------------------------------------------------------

const JSON_SRC: &str = r#"
import "std.json" as json

type Repo = { url :: Str, branch :: Str }

fn strict(s :: Str, required :: List[Str]) -> Result[Repo, Str] {
  json.parse_strict(s, required)
}
"#;

#[test]
fn json_parse_strict_fails_on_missing_field() {
    let src = r#"{"url": "https://example.com"}"#;
    let v = run(JSON_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("url".into()), Value::Str("branch".into())]),
    ]);
    let detail = err_msg(&v);
    assert!(detail.contains("branch"), "should name missing field: {detail}");
}

#[test]
fn json_parse_strict_passes_when_all_present() {
    let src = r#"{"url": "https://example.com", "branch": "main"}"#;
    let v = run(JSON_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("url".into()), Value::Str("branch".into())]),
    ]);
    let _ = ok(&v);
}

#[test]
fn json_parse_strict_fails_when_top_level_is_not_an_object() {
    // A bare JSON array can't have named fields. parse_strict
    // surfaces this as Err rather than crashing.
    let src = "[1, 2, 3]";
    let v = run(JSON_SRC, "strict", vec![
        Value::Str(src.into()),
        Value::List(vec![Value::Str("url".into())]),
    ]);
    let _ = err_msg(&v);
}
