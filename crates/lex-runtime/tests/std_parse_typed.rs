//! `check_and_rewrite_program` — full type-driven `parse[T]` (#168).
//!
//! When the type-checker can see that a `<module>.parse(s)` call
//! returns `Result[Record{...}, _]`, the rewrite pass mutates
//! the AST so the bytecode emits `<module>.parse_strict(s, [fields])`
//! instead. The runtime then validates required fields before
//! returning `Ok`, replacing the pre-#168 silent-incomplete-record
//! behavior with a proper `Err`.
//!
//! Pinned on the rubric (formerly OSS Auditor) repro.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::sync::Arc;

fn run_with_rewrite(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let mut stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
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

fn ok(v: &Value) -> &Value {
    match v {
        Value::Variant { name, args } if name == "Ok" => &args[0],
        other => panic!("expected Ok, got {other:?}"),
    }
}

const TOML_SRC: &str = r#"
import "std.toml" as toml

type Manifest = { license :: Str, version :: Str }

# Plain parse — but the rewrite turns this into parse_strict.
fn extract(s :: Str) -> Result[Manifest, Str] { toml.parse(s) }
"#;

#[test]
fn toml_parse_t_validates_required_fields_after_rewrite() {
    // The rubric scenario: TOML has `version` but no `license`.
    // Pre-#168, plain `toml.parse[Manifest]` returned
    // `Ok(incomplete)` and panicked at field access. After the
    // rewrite, the type-checker injects the required-fields list
    // and the runtime returns Err naming the missing field.
    let src = "version = \"0.1.0\"\n";
    let v = run_with_rewrite(TOML_SRC, "extract", vec![Value::Str(src.into())]);
    let detail = err_msg(&v);
    assert!(detail.contains("missing required field"),
        "rewritten parse should reject incomplete records: {detail}");
    assert!(detail.contains("license"),
        "error should name the missing field: {detail}");
}

#[test]
fn toml_parse_t_passes_when_all_fields_present() {
    let src = "license = \"EUPL-1.2\"\nversion = \"0.1.0\"\n";
    let v = run_with_rewrite(TOML_SRC, "extract", vec![Value::Str(src.into())]);
    let m = ok(&v);
    match m {
        Value::Record(fields) => {
            assert!(fields.contains_key("license"));
            assert!(fields.contains_key("version"));
        }
        other => panic!("expected Record, got {other:?}"),
    }
}

const YAML_SRC: &str = r#"
import "std.yaml" as yaml

type Cargo = { name :: Str, version :: Str }

fn extract(s :: Str) -> Result[Cargo, Str] { yaml.parse(s) }
"#;

#[test]
fn yaml_parse_t_validates_required_fields_after_rewrite() {
    let src = "name: foo\n";
    let v = run_with_rewrite(YAML_SRC, "extract", vec![Value::Str(src.into())]);
    let detail = err_msg(&v);
    assert!(detail.contains("version"),
        "yaml rewrite should also reject missing field: {detail}");
}

const JSON_SRC: &str = r#"
import "std.json" as json

type Person = { name :: Str, age :: Int }

fn extract(s :: Str) -> Result[Person, Str] { json.parse(s) }
"#;

#[test]
fn json_parse_t_validates_required_fields_after_rewrite() {
    let src = r#"{"name": "alice"}"#;
    let v = run_with_rewrite(JSON_SRC, "extract", vec![Value::Str(src.into())]);
    let detail = err_msg(&v);
    assert!(detail.contains("age"),
        "json rewrite should also reject missing field: {detail}");
}

const NESTED_MATCH_SRC: &str = r#"
import "std.toml" as toml

type Manifest = { license :: Str, version :: Str }

# The rubric scenario, expressed with the let-annotation idiom
# Lex's parser actually supports (a let-binding with type
# annotation pins T; pattern-level annotations like
# `Ok(m :: Manifest)` aren't accepted by the grammar).
fn extract(s :: Str) -> Str {
  let parsed :: Result[Manifest, Str] := toml.parse(s)
  match parsed {
    Ok(m) => m.license,
    Err(e) => e,
  }
}
"#;

#[test]
fn toml_parse_t_inferred_via_let_annotation() {
    // Pre-#168, this would silently produce an incomplete record
    // and panic on `m.license` access. After the rewrite, the
    // parse itself returns Err naming the missing field and the
    // match arm forwards it as the function's Str result.
    let src = "version = \"0.1.0\"\n";
    let v = run_with_rewrite(NESTED_MATCH_SRC, "extract",
        vec![Value::Str(src.into())]);
    match &v {
        Value::Str(s) => {
            assert!(s.contains("missing required field") || s.contains("license"),
                "expected an error path: {s}");
        }
        other => panic!("expected Str, got {other:?}"),
    }
}

#[test]
fn untyped_parse_call_is_not_rewritten() {
    // No type annotation guiding T → no rewrite → plain parse
    // semantics preserved. This pins that the rewrite is purely
    // type-driven and doesn't change other code paths.
    let src = r#"
import "std.toml" as toml

# Result type is Result[T, Str] with T as a free variable; the
# rewrite pass only fires when T resolves to a known Record. Here
# nothing constrains T, so parse returns Ok(whatever-was-parsed).
fn passthrough(s :: Str) -> Result[a, Str] { toml.parse(s) }
"#;
    let prog = parse_source(src).expect("parse");
    let mut stages = canonicalize_program(&prog);
    let pt = lex_types::check_and_rewrite_program(&mut stages)
        .expect("type-check");
    assert!(pt.parse_required_fields.is_empty(),
        "untyped parse call should not be in the rewrite map");
}
