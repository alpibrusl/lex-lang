//! Nested `parse_strict` (Rubric port follow-up to #168).
//!
//! `parse_strict` originally only validated top-level required
//! fields. The Rubric port hit two real cases this didn't cover:
//!
//! * **`pyproject.toml`** — `[project].license` is a nested
//!   required field. Workaround was regex extraction.
//! * **GitHub Actions workflows** — most steps require either
//!   `run` xor `uses`, which itself sits under `jobs.<job>.steps[]`.
//!
//! This slice extends the required-fields list to accept dotted
//! paths so callers can write `["project.license"]` and have the
//! validator descend into nested objects. List indexing and `xor`
//! are out of scope for now.
//!
//! Type-driven derivation from `T` (the cleaner endgame called out
//! in the parse_strict comment) is still future work.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return Err(format!("type errors: {errs:#?}"));
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).map_err(|e| format!("{e}"))
}

const TOML_SRC: &str = r#"
import "std.toml" as toml
fn parse_proj(s :: Str) -> Result[Map[Str, Json], Str] {
  toml.parse_strict(s, ["project.name", "project.license"])
}
"#;

#[test]
fn nested_dotted_path_passes_when_all_present() {
    let toml_src = r#"
[project]
name = "foo"
license = "MIT"
"#;
    let v = run(TOML_SRC, "parse_proj", vec![Value::Str(toml_src.into())]).unwrap();
    // Result::Ok wrapper; we only need to check it's not Err.
    assert!(matches!(&v, Value::Variant { name, .. } if name == "Ok"),
        "expected Ok, got: {v:?}");
}

#[test]
fn nested_dotted_path_reports_missing_inner_field() {
    let toml_src = r#"
[project]
name = "foo"
"#;
    let v = run(TOML_SRC, "parse_proj", vec![Value::Str(toml_src.into())]).unwrap();
    let err = match v {
        Value::Variant { name, args } if name == "Err" => args.into_iter().next().unwrap(),
        other => panic!("expected Err, got: {other:?}"),
    };
    let msg = format!("{err:?}");
    assert!(msg.contains("project.license"),
        "error should name the missing dotted path; got: {msg}");
    assert!(!msg.contains("project.name"),
        "present dotted path should not appear in error; got: {msg}");
}

#[test]
fn missing_intermediate_object_reports_full_path() {
    // No `[project]` section at all → both inner fields missing.
    let v = run(TOML_SRC, "parse_proj", vec![Value::Str("name = \"top\"\n".into())]).unwrap();
    let err = match v {
        Value::Variant { name, args } if name == "Err" => args.into_iter().next().unwrap(),
        other => panic!("expected Err, got: {other:?}"),
    };
    let msg = format!("{err:?}");
    assert!(msg.contains("project.name") && msg.contains("project.license"),
        "both missing dotted paths should be listed; got: {msg}");
}

#[test]
fn intermediate_non_object_is_treated_as_missing() {
    // `project = "MIT"` makes the intermediate a string, not an
    // object. Walking through it should fail cleanly rather than
    // panic or accept the descent.
    let toml_src = r#"
project = "MIT"
"#;
    let v = run(TOML_SRC, "parse_proj", vec![Value::Str(toml_src.into())]).unwrap();
    let err = match v {
        Value::Variant { name, args } if name == "Err" => args.into_iter().next().unwrap(),
        other => panic!("expected Err on string-where-object, got: {other:?}"),
    };
    let msg = format!("{err:?}");
    assert!(msg.contains("project.name") || msg.contains("project.license"),
        "should report a dotted-path miss; got: {msg}");
}

#[test]
fn deep_three_level_path_works() {
    let src = r#"
import "std.json" as json
fn parse_deep(s :: Str) -> Result[Map[Str, Json], Str] {
  json.parse_strict(s, ["a.b.c"])
}
"#;
    // Present case.
    let json = r#"{ "a": { "b": { "c": 1 } } }"#;
    let v = run(src, "parse_deep", vec![Value::Str(json.into())]).unwrap();
    assert!(matches!(&v, Value::Variant { name, .. } if name == "Ok"));

    // Missing innermost.
    let json = r#"{ "a": { "b": {} } }"#;
    let v = run(src, "parse_deep", vec![Value::Str(json.into())]).unwrap();
    let err = match v {
        Value::Variant { name, args } if name == "Err" => args.into_iter().next().unwrap(),
        other => panic!("expected Err, got: {other:?}"),
    };
    assert!(format!("{err:?}").contains("a.b.c"));
}

#[test]
fn top_level_field_still_works() {
    // Backward-compat: a path with no dot still works exactly as
    // before — that's the daily-driver case from #168.
    let src = r#"
import "std.json" as json
fn check_name(s :: Str) -> Result[Map[Str, Json], Str] {
  json.parse_strict(s, ["name"])
}
"#;
    let v = run(src, "check_name", vec![Value::Str(r#"{"name": "x"}"#.into())]).unwrap();
    assert!(matches!(&v, Value::Variant { name, .. } if name == "Ok"));

    let v = run(src, "check_name", vec![Value::Str(r#"{"version": "1"}"#.into())]).unwrap();
    let err = match v {
        Value::Variant { name, args } if name == "Err" => args.into_iter().next().unwrap(),
        _ => panic!("expected Err"),
    };
    assert!(format!("{err:?}").contains("name"));
}

#[test]
fn literal_dot_in_field_name_via_escape() {
    // `"package\\.json"` in a Lex source becomes `package\.json`
    // at runtime; the parse_strict path handler treats `\.` as a
    // literal dot, so the descent doesn't split. Field names
    // containing dots are rare but legitimate (e.g. domains).
    let src = r#"
import "std.json" as json
fn check(s :: Str) -> Result[Map[Str, Json], Str] {
  json.parse_strict(s, ["package\\.json"])
}
"#;
    // `package.json` is a top-level key (with a literal dot).
    let v = run(src, "check", vec![
        Value::Str(r#"{ "package.json": "{}" }"#.into()),
    ]).unwrap();
    let is_ok = matches!(&v, Value::Variant { name, .. } if name == "Ok");
    assert!(is_ok, "literal-dot key with `\\.` escape should pass; got: {v:?}");
}

#[test]
fn yaml_nested_required_works_too() {
    // YAML support uses the same `check_required_fields` plumbing,
    // so dotted paths just work — pin it explicitly so a future
    // refactor that splits the implementations doesn't regress.
    let src = r#"
import "std.yaml" as yaml
fn check(s :: Str) -> Result[Map[Str, Json], Str] {
  yaml.parse_strict(s, ["jobs.test"])
}
"#;
    let yaml = "jobs:\n  test:\n    runs-on: ubuntu-latest\n";
    let v = run(src, "check", vec![Value::Str(yaml.into())]).unwrap();
    assert!(matches!(&v, Value::Variant { name, .. } if name == "Ok"));
}
