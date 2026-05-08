//! End-to-end tests for `std.cli` (Rubric port).
//!
//! Exercises the language-level surface — building a CliSpec with
//! `cli.flag` / `cli.option` / `cli.positional` / `cli.spec`, then
//! parsing argv with `cli.parse` and consuming the result. Covers
//! the cases the Rubric CLI uses (six subcommands, mixed flags +
//! options, JSON envelope, ACLI introspection).

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

fn list_str(items: &[&str]) -> Value {
    Value::List(items.iter().map(|s| Value::Str((*s).into())).collect())
}

#[test]
fn build_and_parse_simple_cli() {
    let src = r#"
import "std.cli" as cli

fn build_spec() -> Json {
  cli.spec("rubric", "Rubric CLI", [
    cli.flag("verbose", Some("v"), "show debug output"),
    cli.option("output", Some("o"), "write report", None),
    cli.positional("path", "directory to scan", true),
  ], [])
}

fn parse_argv(argv :: List[Str]) -> Result[Json, Str] {
  cli.parse(build_spec(), argv)
}
"#;
    let v = run(src, "parse_argv", vec![list_str(&["./src", "--verbose"])]).unwrap();
    let parsed = match v {
        Value::Variant { name, mut args } if name == "Ok" => args.remove(0),
        other => panic!("expected Ok, got: {other:?}"),
    };
    let json = parsed.to_json();
    assert_eq!(json["positionals"]["path"], "./src");
    assert_eq!(json["flags"]["verbose"], true);
}

#[test]
fn six_subcommands_dispatch() {
    // Mirror Rubric's shape: top-level + 6 subcommands. The path of
    // the resolved command should be ["rubric", "<sub>"].
    let src = r#"
import "std.cli" as cli

fn build_spec() -> Json {
  cli.spec("rubric", "audit your repo", [], [
    cli.spec("scan",   "scan a directory", [
      cli.positional("path", "dir", true),
    ], []),
    cli.spec("init",   "initialise",      [], []),
    cli.spec("report", "emit report",     [], []),
    cli.spec("score",  "score a project", [], []),
    cli.spec("badge",  "render a badge",  [], []),
    cli.spec("ci",     "ci helpers",      [], []),
  ])
}

fn parse_argv(argv :: List[Str]) -> Result[Json, Str] {
  cli.parse(build_spec(), argv)
}
"#;
    for sub in ["scan", "init", "report", "score", "badge", "ci"] {
        // `scan` needs a path positional; everything else takes none.
        let argv = if sub == "scan" {
            list_str(&[sub, "./somewhere"])
        } else {
            list_str(&[sub])
        };
        let v = run(src, "parse_argv", vec![argv]).unwrap();
        let parsed = match v {
            Value::Variant { name, mut args } if name == "Ok" => args.remove(0).to_json(),
            other => panic!("expected Ok for `{sub}`, got: {other:?}"),
        };
        let cmd = parsed["command"].as_array().unwrap();
        assert_eq!(cmd[0], "rubric");
        assert_eq!(cmd[1], sub, "wrong subcommand for input `{sub}`");
    }
}

#[test]
fn missing_required_positional_returns_err() {
    let src = r#"
import "std.cli" as cli

fn parse_argv(argv :: List[Str]) -> Result[Json, Str] {
  let s := cli.spec("p", "prog", [
    cli.positional("input", "input file", true),
  ], [])
  cli.parse(s, argv)
}
"#;
    let v = run(src, "parse_argv", vec![Value::List(vec![])]).unwrap();
    let err = match v {
        Value::Variant { name, mut args } if name == "Err" => args.remove(0),
        other => panic!("expected Err, got: {other:?}"),
    };
    let msg = match err { Value::Str(s) => s, _ => panic!() };
    assert!(msg.contains("missing required") && msg.contains("input"),
        "expected missing-positional error; got: {msg}");
}

#[test]
fn json_envelope_wraps_command_data() {
    let src = r#"
import "std.cli" as cli
fn make_envelope(ok :: Bool, name :: Str) -> Json {
  cli.envelope(ok, name, [1, 2, 3])
}
"#;
    let v = run(src, "make_envelope", vec![
        Value::Bool(true), Value::Str("rubric".into()),
    ]).unwrap();
    let env = v.to_json();
    assert_eq!(env["ok"], true);
    assert_eq!(env["command"], "rubric");
    assert_eq!(env["data"], serde_json::json!([1, 2, 3]));
}

#[test]
fn describe_returns_machine_readable_spec() {
    let src = r#"
import "std.cli" as cli

fn describe_self() -> Json {
  let s := cli.spec("rubric", "outer", [
    cli.flag("verbose", Some("v"), ""),
  ], [
    cli.spec("scan", "scan dir", [], []),
  ])
  cli.describe(s)
}
"#;
    let v = run(src, "describe_self", vec![]).unwrap();
    let d = v.to_json();
    assert_eq!(d["name"], "rubric");
    assert_eq!(d["help"], "outer");
    let subs = d["subcommands"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["name"], "scan");
}

#[test]
fn help_text_includes_args_and_subs() {
    let src = r#"
import "std.cli" as cli

fn render_help() -> Str {
  let s := cli.spec("rubric", "audit a project", [
    cli.flag("verbose", Some("v"), "noisy"),
    cli.positional("path", "directory", true),
  ], [
    cli.spec("scan", "scan a directory", [], []),
  ])
  cli.help(s)
}
"#;
    let v = run(src, "render_help", vec![]).unwrap();
    let s = match v { Value::Str(x) => x, _ => panic!() };
    assert!(s.contains("rubric"));
    assert!(s.contains("audit a project"));
    assert!(s.contains("--verbose") && s.contains("-v"));
    assert!(s.contains("<path>"));
    assert!(s.contains("scan"));
}

#[test]
fn double_dash_separator_collects_remaining() {
    let src = r#"
import "std.cli" as cli

fn parse_argv(argv :: List[Str]) -> Result[Json, Str] {
  let s := cli.spec("p", "", [
    cli.positional("path", "", true),
  ], [])
  cli.parse(s, argv)
}
"#;
    let v = run(src, "parse_argv", vec![
        list_str(&["src", "--", "--would-be-flag", "rest"]),
    ]).unwrap();
    let parsed = match v {
        Value::Variant { name, mut args } if name == "Ok" => args.remove(0).to_json(),
        other => panic!("expected Ok, got: {other:?}"),
    };
    let remaining = parsed["remaining"].as_array().unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0], "--would-be-flag");
    assert_eq!(remaining[1], "rest");
}

#[test]
fn option_with_default_when_absent_is_present_in_parsed() {
    let src = r#"
import "std.cli" as cli

fn parse_argv(argv :: List[Str]) -> Result[Json, Str] {
  let s := cli.spec("p", "", [
    cli.option("level", None, "verbosity", Some("info")),
  ], [])
  cli.parse(s, argv)
}
"#;
    let v = run(src, "parse_argv", vec![Value::List(vec![])]).unwrap();
    let parsed = match v {
        Value::Variant { name, mut args } if name == "Ok" => args.remove(0).to_json(),
        other => panic!("expected Ok, got: {other:?}"),
    };
    assert_eq!(parsed["options"]["level"], "info");
}
