//! Integration tests for `std.regex`. Closes #96.

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

fn s(v: Value) -> String {
    match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    }
}

fn b(v: Value) -> bool {
    match v {
        Value::Bool(b) => b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

fn list_of_strs(v: Value) -> Vec<String> {
    match v {
        Value::List(items) => items
            .into_iter()
            .map(|i| match i {
                Value::Str(s) => s,
                other => panic!("expected Str in list, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List, got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.regex" as regex
import "std.list" as list

# Each helper takes the pattern + input, compiles inline, and either
# returns a sensible default on Err or matches the test contract.

fn match_p(pat :: Str, s :: Str) -> Bool {
  match regex.compile(pat) {
    Ok(r)  => regex.is_match(r, s),
    Err(_) => false,
  }
}

fn replace_p(pat :: Str, s :: Str, rep :: Str) -> Str {
  match regex.compile(pat) {
    Ok(r)  => regex.replace_all(r, s, rep),
    Err(_) => s,
  }
}

fn split_p(pat :: Str, s :: Str) -> List[Str] {
  match regex.compile(pat) {
    Ok(r)  => regex.split(r, s),
    Err(_) => [s],
  }
}

fn first_match_text(pat :: Str, s :: Str) -> Str {
  match regex.compile(pat) {
    Ok(r)  => match regex.find(r, s) {
      Some(m) => m.text,
      None    => "<no match>",
    },
    Err(_) => "<bad pattern>",
  }
}

fn first_capture(pat :: Str, s :: Str) -> Str {
  match regex.compile(pat) {
    Ok(r)  => match regex.find(r, s) {
      Some(m) => match list.head(m.groups) {
        Some(g) => g,
        None    => "<no group>",
      },
      None => "<no match>",
    },
    Err(_) => "<bad pattern>",
  }
}

fn count_matches(pat :: Str, s :: Str) -> Int {
  match regex.compile(pat) {
    Ok(r)  => list.len(regex.find_all(r, s)),
    Err(_) => 0 - 1,
  }
}

fn compile_validates(pat :: Str) -> Bool {
  match regex.compile(pat) {
    Ok(_)  => true,
    Err(_) => false,
  }
}
"#;

#[test]
fn is_match_basic() {
    assert!(b(run(SRC, "match_p", vec![
        Value::Str(r"\d+".into()),
        Value::Str("abc 123 def".into()),
    ])));
    assert!(!b(run(SRC, "match_p", vec![
        Value::Str(r"^\d+$".into()),
        Value::Str("abc 123 def".into()),
    ])));
}

#[test]
fn replace_all_substitutes_every_occurrence() {
    let v = run(SRC, "replace_p", vec![
        Value::Str(r"\d+".into()),
        Value::Str("a 1 b 22 c 333".into()),
        Value::Str("N".into()),
    ]);
    assert_eq!(s(v), "a N b N c N");
}

#[test]
fn split_returns_pieces() {
    let v = run(SRC, "split_p", vec![
        Value::Str(r"\s*,\s*".into()),
        Value::Str("a,  b,c , d".into()),
    ]);
    assert_eq!(list_of_strs(v), vec!["a", "b", "c", "d"]);
}

#[test]
fn find_returns_first_match_text() {
    let v = run(SRC, "first_match_text", vec![
        Value::Str(r"[A-Z][a-z]+".into()),
        Value::Str("alpha Beta gamma Delta".into()),
    ]);
    assert_eq!(s(v), "Beta");
}

#[test]
fn find_returns_none_when_no_match() {
    let v = run(SRC, "first_match_text", vec![
        Value::Str(r"\d+".into()),
        Value::Str("only letters here".into()),
    ]);
    assert_eq!(s(v), "<no match>");
}

#[test]
fn find_all_count_matches() {
    let v = run(SRC, "count_matches", vec![
        Value::Str(r"\b[a-z]+\b".into()),
        Value::Str("one TWO three FOUR five".into()),
    ]);
    assert_eq!(v, Value::Int(3));
}

#[test]
fn find_returns_capture_groups() {
    let v = run(SRC, "first_capture", vec![
        Value::Str(r"name=(\w+)".into()),
        Value::Str("user name=alice end".into()),
    ]);
    assert_eq!(s(v), "alice");
}

#[test]
fn compile_succeeds_for_valid_pattern() {
    assert!(b(run(SRC, "compile_validates", vec![
        Value::Str(r"^\d+$".into()),
    ])));
}

#[test]
fn compile_returns_err_for_invalid_pattern() {
    // Unbalanced bracket — invalid regex.
    assert!(!b(run(SRC, "compile_validates", vec![
        Value::Str(r"[abc".into()),
    ])));
}

#[test]
fn replace_with_backrefs() {
    let v = run(SRC, "replace_p", vec![
        Value::Str(r"(\w+)@(\w+)".into()),
        Value::Str("alice@example bob@test".into()),
        Value::Str("$2.$1".into()),
    ]);
    assert_eq!(s(v), "example.alice test.bob");
}
