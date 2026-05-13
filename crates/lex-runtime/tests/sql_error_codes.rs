//! Integration tests for #380: structured SqlError on the `Err` side
//! of every `std.sql` Result. The runtime populates `code` with the
//! symbolic SQLite error name (`SQLITE_CONSTRAINT_PRIMARYKEY`, …) or
//! the 5-character Postgres SQLSTATE; `message` always carries the
//! human-readable string; `detail` carries driver detail if present.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn policy_sql_only() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["sql".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, func: &str) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy_sql_only());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, vec![]).expect("vm")
}

/// Pull a `SqlError` record's three string-shaped fields out of a
/// Lex `Value::Record`. `message` is always Str; `code` and `detail`
/// are `Option[Str]` so they come back as `Variant{Some|None}`.
fn unpack_sql_error(v: &Value) -> (String, Option<String>, Option<String>) {
    let rec = match v {
        Value::Record(r) => r,
        other => panic!("expected SqlError record, got {other:?}"),
    };
    let message = match rec.get("message") {
        Some(Value::Str(s)) => s.clone(),
        other => panic!("expected message :: Str, got {other:?}"),
    };
    let read_opt = |key: &str| match rec.get(key) {
        Some(Value::Variant { name, args }) if name == "Some" && args.len() == 1 => {
            match &args[0] {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            }
        }
        Some(Value::Variant { name, .. }) if name == "None" => None,
        _ => None,
    };
    (message, read_opt("code"), read_opt("detail"))
}

#[test]
fn sqlite_primary_key_violation_populates_code() {
    // Insert twice into a PK column — second insert trips
    // SQLITE_CONSTRAINT_PRIMARYKEY. Returns the second exec's Err
    // verbatim so the test can inspect the SqlError shape.
    let src = r#"
import "std.sql" as sql

fn force_pk_violation() -> [sql, fs_write] Result[Int, SqlError] {
  match sql.open(":memory:") {
    Err(e) => Err(e),
    Ok(db) => {
      let _ := match sql.exec(db, "CREATE TABLE t(id INTEGER PRIMARY KEY)", []) {
        Ok(_) => 0, Err(_) => 0 - 1
      }
      let _ := match sql.exec(db, "INSERT INTO t(id) VALUES (1)", []) {
        Ok(_) => 0, Err(_) => 0 - 1
      }
      sql.exec(db, "INSERT INTO t(id) VALUES (1)", [])
    }
  }
}
"#;
    let result = run(src, "force_pk_violation");
    let inner = match result {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => {
            args.into_iter().next().unwrap()
        }
        other => panic!("expected Err(SqlError), got {other:?}"),
    };
    let (message, code, _detail) = unpack_sql_error(&inner);
    assert!(
        message.contains("sql.exec") && message.to_lowercase().contains("constraint"),
        "message should mention the failing op + constraint kind: {message:?}"
    );
    assert_eq!(
        code.as_deref(),
        Some("SQLITE_CONSTRAINT_PRIMARYKEY"),
        "expected SQLite extended-code symbolic name; full error: \
         message={message:?}, code={code:?}"
    );
}

#[test]
fn sqlite_unique_violation_populates_code() {
    // UNIQUE constraint on a non-PK column trips
    // SQLITE_CONSTRAINT_UNIQUE — a different extended code than the
    // primary-key path above. Both must be distinguishable from
    // string-parsing the message.
    let src = r#"
import "std.sql" as sql

fn force_unique_violation() -> [sql, fs_write] Result[Int, SqlError] {
  match sql.open(":memory:") {
    Err(e) => Err(e),
    Ok(db) => {
      let _ := match sql.exec(db, "CREATE TABLE t(id INTEGER, name TEXT UNIQUE)", []) {
        Ok(_) => 0, Err(_) => 0 - 1
      }
      let _ := match sql.exec(db, "INSERT INTO t(id, name) VALUES (1, 'alice')", []) {
        Ok(_) => 0, Err(_) => 0 - 1
      }
      sql.exec(db, "INSERT INTO t(id, name) VALUES (2, 'alice')", [])
    }
  }
}
"#;
    let result = run(src, "force_unique_violation");
    let inner = match result {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => {
            args.into_iter().next().unwrap()
        }
        other => panic!("expected Err(SqlError), got {other:?}"),
    };
    let (_message, code, _detail) = unpack_sql_error(&inner);
    assert_eq!(
        code.as_deref(),
        Some("SQLITE_CONSTRAINT_UNIQUE"),
        "expected SQLITE_CONSTRAINT_UNIQUE; got {code:?}"
    );
}

#[test]
fn sqlite_syntax_error_message_is_populated() {
    // A statement the SQLite parser rejects produces a SqlError where
    // `code` is some SQLite generic error and `message` is non-empty.
    // We assert weakly on `code` (just that it's populated) since the
    // specific SQLite extended code for parse errors varies.
    let src = r#"
import "std.sql" as sql

fn bad_syntax() -> [sql, fs_write] Result[List[{ x :: Int }], SqlError] {
  match sql.open(":memory:") {
    Err(e) => Err(e),
    Ok(db) => sql.query(db, "SELECT FROM where junk", []),
  }
}
"#;
    let result = run(src, "bad_syntax");
    let inner = match result {
        Value::Variant { name, args } if name == "Err" && args.len() == 1 => {
            args.into_iter().next().unwrap()
        }
        other => panic!("expected Err(SqlError), got {other:?}"),
    };
    let (message, code, _detail) = unpack_sql_error(&inner);
    assert!(!message.is_empty(), "message must always be populated");
    assert!(message.contains("sql.query"), "message should name the op: {message:?}");
    // We deliberately do not pin the exact code — every SQLite version
    // is allowed to relabel parse errors. We just confirm SOME code
    // shows up, so downstream code can branch on it.
    assert!(
        code.is_some(),
        "expected a non-None `code` even for parse errors; got {code:?}"
    );
}
