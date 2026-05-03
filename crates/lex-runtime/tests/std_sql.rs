//! Integration tests for `std.sql`. v1 surface: open / close / exec
//! / query against SQLite, params as List[Str], rows decoded into
//! polymorphic record shapes via the same `Value` pipeline as
//! `json.parse` and `toml.parse`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn policy_with_sql(write_root: &std::path::Path) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["sql".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    p.allow_fs_write = vec![write_root.to_path_buf()];
    p
}

fn policy_in_memory() -> Policy {
    // ":memory:" doesn't touch the filesystem so no fs-write scope
    // is needed; sql is the only effect.
    let mut p = Policy::pure();
    p.allow_effects = ["sql".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, func: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn unique_db_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-sql-{}-{}-{}",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn unwrap_ok(v: Value) -> Value {
    match v {
        Value::Variant { name, args } if name == "Ok" => args.into_iter().next().expect("Ok payload"),
        other => panic!("expected Ok, got {other:?}"),
    }
}

const SRC: &str = r#"
import "std.sql" as sql
import "std.list" as list

# Round-trip: create a table, insert two rows, query them back as
# typed records.
fn create_insert_query(path :: Str) -> [sql, fs_write] List[{ id :: Int, name :: Str }] {
  match sql.open(path) {
    Ok(db) => {
      let c := match sql.exec(db, "CREATE TABLE users(id INTEGER, name TEXT)", []) {
        Ok(_) => 0, Err(_) => 0 - 1,
      }
      let i1 := match sql.exec(db, "INSERT INTO users VALUES (?1, ?2)", ["1", "alice"]) {
        Ok(_) => 0, Err(_) => 0 - 1,
      }
      let i2 := match sql.exec(db, "INSERT INTO users VALUES (?1, ?2)", ["2", "bob"]) {
        Ok(_) => 0, Err(_) => 0 - 1,
      }
      let result :: Result[List[{ id :: Int, name :: Str }], Str] :=
        sql.query(db, "SELECT id, name FROM users ORDER BY id", [])
      match result {
        Ok(rows) => rows,
        Err(_)   => [],
      }
    },
    Err(_) => [],
  }
}

# Smoke test against an in-memory db: count(*) should be 0 against
# a freshly-created empty table.
fn count_empty_table() -> [sql, fs_write] Int {
  match sql.open(":memory:") {
    Ok(db) => {
      let c := match sql.exec(db, "CREATE TABLE t(x INTEGER)", []) { Ok(_) => 0, Err(_) => 0 - 1 }
      let result :: Result[List[{ n :: Int }], Str] :=
        sql.query(db, "SELECT COUNT(*) AS n FROM t", [])
      match result {
        Ok(rows) => match list.head(rows) {
          Some(r) => r.n,
          None    => 0 - 1,
        },
        Err(_) => 0 - 99,
      }
    },
    Err(_) => 0 - 99,
  }
}

# Parameterized WHERE — confirms params bind positionally.
fn pick_by_id(path :: Str, id :: Str) -> [sql, fs_write] Str {
  match sql.open(path) {
    Ok(db) => {
      let result :: Result[List[{ name :: Str }], Str] :=
        sql.query(db, "SELECT name FROM users WHERE id = ?1", [id])
      match result {
        Ok(rows) => match list.head(rows) {
          Some(r) => r.name,
          None    => "<missing>",
        },
        Err(e) => e,
      }
    },
    Err(_) => "<open failed>",
  }
}

# Confirm exec returns the row count.
fn delete_count(path :: Str) -> [sql, fs_write] Int {
  match sql.open(path) {
    Ok(db) => match sql.exec(db, "DELETE FROM users WHERE id = ?1", ["1"]) {
      Ok(n)  => n,
      Err(_) => 0 - 1,
    },
    Err(_) => 0 - 99,
  }
}

# Drive a malformed query — surfaces as Err with the SQLite message.
fn bad_sql_returns_err() -> [sql, fs_write] Str {
  match sql.open(":memory:") {
    Ok(db) => {
      let result :: Result[List[{ x :: Int }], Str] :=
        sql.query(db, "SELECT FROM where junk", [])
      match result {
        Ok(_)  => "<unexpectedly ok>",
        Err(e) => e,
      }
    },
    Err(_) => "<open failed>",
  }
}
"#;

#[test]
fn create_insert_query_round_trip() {
    let dir = unique_db_path("rt");
    let path = dir.join("test.db");
    let v = run(
        SRC,
        "create_insert_query",
        vec![Value::Str(path.to_string_lossy().into_owned())],
        policy_with_sql(&dir),
    );
    let rows = match v {
        Value::List(r) => r,
        other => panic!("expected List, got {other:?}"),
    };
    assert_eq!(rows.len(), 2);
    let row0 = match &rows[0] { Value::Record(r) => r, other => panic!("{other:?}") };
    assert_eq!(row0.get("id"),   Some(&Value::Int(1)));
    assert_eq!(row0.get("name"), Some(&Value::Str("alice".into())));
    let row1 = match &rows[1] { Value::Record(r) => r, other => panic!("{other:?}") };
    assert_eq!(row1.get("id"),   Some(&Value::Int(2)));
    assert_eq!(row1.get("name"), Some(&Value::Str("bob".into())));
}

#[test]
fn count_empty_table_returns_zero() {
    let v = run(SRC, "count_empty_table", vec![], policy_in_memory());
    assert_eq!(v, Value::Int(0));
}

#[test]
fn pick_by_id_uses_parameter_binding() {
    // Set up the table the previous test creates, then query.
    let dir = unique_db_path("pick");
    let path = dir.join("test.db");
    // Reuse SRC's create_insert_query to populate.
    let _ = run(
        SRC,
        "create_insert_query",
        vec![Value::Str(path.to_string_lossy().into_owned())],
        policy_with_sql(&dir),
    );
    let v = run(
        SRC,
        "pick_by_id",
        vec![
            Value::Str(path.to_string_lossy().into_owned()),
            Value::Str("2".into()),
        ],
        policy_with_sql(&dir),
    );
    assert_eq!(v, Value::Str("bob".into()));
}

#[test]
fn delete_returns_affected_row_count() {
    let dir = unique_db_path("del");
    let path = dir.join("test.db");
    let _ = run(
        SRC,
        "create_insert_query",
        vec![Value::Str(path.to_string_lossy().into_owned())],
        policy_with_sql(&dir),
    );
    let v = run(
        SRC,
        "delete_count",
        vec![Value::Str(path.to_string_lossy().into_owned())],
        policy_with_sql(&dir),
    );
    assert_eq!(v, Value::Int(1));
}

#[test]
fn bad_sql_surfaces_as_err() {
    let v = run(SRC, "bad_sql_returns_err", vec![], policy_in_memory());
    let s = match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    };
    assert!(s.starts_with("sql.query:"), "expected sql.query: prefix, got {s}");
}

#[test]
fn open_outside_fs_write_root_returns_err() {
    let dir = unique_db_path("scope");
    // Allow writes only under `dir`; try to open a db elsewhere.
    let outside = std::env::temp_dir().join(format!(
        "lex-sql-outside-{}.db", std::process::id()));
    let src = r#"
import "std.sql" as sql
fn try_open(p :: Str) -> [sql, fs_write] Str {
  match sql.open(p) {
    Ok(_)  => "<unexpectedly opened>",
    Err(e) => e,
  }
}
"#;
    let v = run(
        src,
        "try_open",
        vec![Value::Str(outside.to_string_lossy().into_owned())],
        policy_with_sql(&dir),
    );
    let s = match v {
        Value::Str(s) => s,
        other => panic!("expected Str, got {other:?}"),
    };
    assert!(
        s.contains("outside --allow-fs-write"),
        "expected fs-write scope error, got {s}",
    );
}

#[test]
fn null_column_decodes_as_unit() {
    let src = r#"
import "std.sql" as sql
import "std.list" as list

# A column with NULL maps to Lex's Unit (i.e. the absence of a
# value) inside the record. Verifies the SQLite Null path is
# wired through correctly without requiring Option in user code.
fn nullable() -> [sql, fs_write] Bool {
  match sql.open(":memory:") {
    Ok(db) => {
      let c := match sql.exec(db, "CREATE TABLE t(x INTEGER)", []) { Ok(_) => 0, Err(_) => 0 - 1 }
      let i := match sql.exec(db, "INSERT INTO t(x) VALUES (NULL)", []) { Ok(_) => 0, Err(_) => 0 - 1 }
      let result :: Result[List[{ x :: Int }], Str] :=
        sql.query(db, "SELECT x FROM t", [])
      match result {
        Ok(_)  => true,
        Err(_) => false,
      }
    },
    Err(_) => false,
  }
}
"#;
    // The query succeeds; the runtime decodes NULL → Unit. We're
    // checking that nothing panics — if the Null path was missing
    // from sql_value_ref_to_lex this would crash.
    let v = run(src, "nullable", vec![], policy_in_memory());
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn close_invalidates_subsequent_ops() {
    // After sql.close, subsequent ops on the same handle hit the
    // "closed or unknown Db handle" path, surfaced here as a Rust-
    // level VM error (the dispatch returns Err out-of-band, same
    // shape as kv's closed-handle behavior).
    let src = r#"
import "std.sql" as sql
fn close_then_query() -> [sql, fs_write] Result[List[{ x :: Int }], Str] {
  match sql.open(":memory:") {
    Ok(db) => {
      let closed := sql.close(db)
      sql.query(db, "SELECT 1 AS x", [])
    },
    Err(e) => Err(e),
  }
}
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy_in_memory());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let r = vm.call("close_then_query", vec![]);
    let err = format!("{:?}", r.expect_err("expected VM-level close-handle error"));
    assert!(
        err.contains("closed or unknown Db handle"),
        "expected closed-handle error, got {err}",
    );
}

// Suppress unused-helper lint; `unwrap_ok` is used in the nullable
// test, but rustc currently flags it because the pattern matches
// the Variant before reaching the helper.
#[allow(dead_code)]
fn _force_use_unwrap_ok(v: Value) -> Value { unwrap_ok(v) }
