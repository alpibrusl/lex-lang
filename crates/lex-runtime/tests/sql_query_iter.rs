//! Integration tests for `sql.query_iter[T]` (#379) — streaming cursor
//! that returns rows one at a time via an mpsc-backed `Iter[T]`.
//!
//! Verified on in-memory SQLite to keep tests fast and dependency-free.
//! Postgres path shares the same dispatch surface (effect call into
//! `sql.query_iter` → `__IterCursor(handle)` → bytecode `iter.next`
//! drives `sql.cursor_next`), so the assertions here also cover the
//! semantic shape the PG handler must preserve.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;

fn policy() -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["sql".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    p
}

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(policy());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

// Seed the in-memory DB with N rows, open a streaming cursor, drain
// the cursor by repeated `iter.next` calls into an accumulator list,
// and return the accumulator.
//
// Manual `iter.next` walk rather than `iter.to_list` because
// `iter.to_list`'s cursor dispatch is the next slice — this test just
// pins the underlying primitive.
const SRC_SEED_AND_DRAIN: &str = r#"
import "std.sql" as sql
import "std.list" as list
import "std.iter" as iter

fn seed(db :: Db, n :: Int) -> [sql] Int {
  let _ := match sql.exec(db, "CREATE TABLE rows(id INTEGER, name TEXT)", []) {
    Ok(_) => 0,
    Err(_) => 0 - 1,
  }
  let _ := match sql.exec(db, "INSERT INTO rows SELECT value, 'r' || value FROM generate_series(0, ?1)", [PInt(n - 1)]) {
    Ok(_) => 0,
    # generate_series isn't available on SQLite — fall back to a small
    # explicit insert loop driven from outside. For this test we just
    # call sql.exec per row before draining.
    Err(_) => 0 - 1,
  }
  0
}

# Insert a single row at id=i with name "r{i}".
fn insert_row(db :: Db, i :: Int, name :: Str) -> [sql] Int {
  match sql.exec(db, "INSERT INTO rows VALUES (?1, ?2)", [PInt(i), PStr(name)]) {
    Ok(n) => n,
    Err(_) => 0 - 1,
  }
}

# Drain a cursor into a List[Str] of `name` fields via repeated
# `iter.next` — terminates on `None`.
fn drain_names(it :: Iter[{ id :: Int, name :: Str }]) -> List[Str] {
  match iter.next(it) {
    None              => [],
    Some((row, rest)) => list.cons(row.name, drain_names(rest)),
  }
}

fn count_via_next(it :: Iter[{ id :: Int, name :: Str }]) -> Int {
  match iter.next(it) {
    None              => 0,
    Some((_, rest))   => 1 + count_via_next(rest),
  }
}

# Drain only the first N rows then stop — exercises early termination,
# which is the streaming benefit over `sql.query` (which would fetch
# every row up-front).
fn first_n_names(it :: Iter[{ id :: Int, name :: Str }], n :: Int) -> List[Str] {
  match n {
    0 => [],
    _ => match iter.next(it) {
      None              => [],
      Some((row, rest)) => list.cons(row.name, first_n_names(rest, n - 1)),
    },
  }
}
"#;

/// Open an in-memory SQLite, run the given test function with the db
/// handle, return its result. Helper hides the open/exec/close
/// boilerplate.
fn with_seeded_db(rows: usize, test_fn: &str, extra_arg: Option<Value>) -> Value {
    let runner_src = format!(
        r#"
{prelude}

fn run_test() -> [sql, fs_write] Result[List[Str], Str] {{
  match sql.open(":memory:") {{
    Err(e) => Err(e),
    Ok(db) => {{
      let _ := match sql.exec(db, "CREATE TABLE rows(id INTEGER, name TEXT)", []) {{
        Ok(_) => 0,
        Err(_) => 0 - 1,
      }}
      Ok({test_invocation})
    }}
  }}
}}

fn run_count() -> [sql, fs_write] Result[Int, Str] {{
  match sql.open(":memory:") {{
    Err(e) => Err(e),
    Ok(db) => {{
      let _ := match sql.exec(db, "CREATE TABLE rows(id INTEGER, name TEXT)", []) {{
        Ok(_) => 0,
        Err(_) => 0 - 1,
      }}
      {seed_loop}
      let result :: Result[Iter[{{ id :: Int, name :: Str }}], Str] :=
        sql.query_iter(db, "SELECT id, name FROM rows ORDER BY id", [])
      match result {{
        Err(e) => Err(e),
        Ok(it) => Ok(count_via_next(it)),
      }}
    }}
  }}
}}
"#,
        prelude = SRC_SEED_AND_DRAIN,
        seed_loop = (0..rows)
            .map(|i| format!(
                r#"      let _ := insert_row(db, {i}, "r{i}")
"#
            ))
            .collect::<String>(),
        test_invocation = match test_fn {
            "drain" => format!(
                r#"{{
        let _ := 0
        {seed}
        let result :: Result[Iter[{{ id :: Int, name :: Str }}], Str] :=
          sql.query_iter(db, "SELECT id, name FROM rows ORDER BY id", [])
        match result {{
          Err(e2) => [e2],
          Ok(it) => drain_names(it),
        }}
      }}"#,
                seed = (0..rows)
                    .map(|i| format!(
                        r#"        let _ := insert_row(db, {i}, "r{i}")
"#
                    ))
                    .collect::<String>(),
            ),
            "first_n" => {
                let n = match extra_arg {
                    Some(Value::Int(n)) => n,
                    _ => 0,
                };
                format!(
                    r#"{{
        let _ := 0
        {seed}
        let result :: Result[Iter[{{ id :: Int, name :: Str }}], Str] :=
          sql.query_iter(db, "SELECT id, name FROM rows ORDER BY id", [])
        match result {{
          Err(e2) => [e2],
          Ok(it) => first_n_names(it, {n}),
        }}
      }}"#,
                    seed = (0..rows)
                        .map(|i| format!(
                            r#"        let _ := insert_row(db, {i}, "r{i}")
"#
                        ))
                        .collect::<String>(),
                )
            }
            _ => panic!("unknown test fn {test_fn}"),
        }
    );
    let entry = if test_fn == "count" { "run_count" } else { "run_test" };
    run(&runner_src, entry, vec![])
}

#[test]
fn drains_all_rows_in_order() {
    let got = with_seeded_db(5, "drain", None);
    // Unwrap Ok(names :: List[Str])
    let names = match got {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => args.into_iter().next().unwrap(),
        other => panic!("expected Ok(List[Str]), got {other:?}"),
    };
    let list = match names {
        Value::List(items) => items,
        other => panic!("expected List, got {other:?}"),
    };
    let expected: Vec<Value> = (0..5)
        .map(|i| Value::Str(format!("r{i}")))
        .collect();
    assert_eq!(list, expected);
}

#[test]
fn count_via_next_matches_row_count() {
    let mut runner_src = String::from(SRC_SEED_AND_DRAIN);
    runner_src.push_str(
        r#"
fn run_count() -> [sql, fs_write] Int {
  match sql.open(":memory:") {
    Err(_) => 0 - 1,
    Ok(db) => {
      let _ := match sql.exec(db, "CREATE TABLE rows(id INTEGER, name TEXT)", []) { Ok(_) => 0, Err(_) => 0 - 1 }
      let _ := insert_row(db, 0, "r0")
      let _ := insert_row(db, 1, "r1")
      let _ := insert_row(db, 2, "r2")
      let _ := insert_row(db, 3, "r3")
      let result :: Result[Iter[{ id :: Int, name :: Str }], Str] :=
        sql.query_iter(db, "SELECT id, name FROM rows ORDER BY id", [])
      match result {
        Err(_) => 0 - 1,
        Ok(it) => count_via_next(it),
      }
    }
  }
}
"#,
    );
    let got = run(&runner_src, "run_count", vec![]);
    assert_eq!(got, Value::Int(4));
}

#[test]
fn early_termination_does_not_block_on_remaining_rows() {
    // Insert 10 rows, ask for just the first 3. The cursor should yield
    // them quickly without blocking; the producer thread will block on
    // a full channel after 64 rows but that's irrelevant for 10.
    let got = with_seeded_db(10, "first_n", Some(Value::Int(3)));
    let names = match got {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => args.into_iter().next().unwrap(),
        other => panic!("expected Ok(List[Str]), got {other:?}"),
    };
    let list = match names {
        Value::List(items) => items,
        other => panic!("expected List, got {other:?}"),
    };
    assert_eq!(
        list,
        vec![
            Value::Str("r0".into()),
            Value::Str("r1".into()),
            Value::Str("r2".into()),
        ]
    );
}

#[test]
fn empty_query_yields_empty_drain() {
    let got = with_seeded_db(0, "drain", None);
    let names = match got {
        Value::Variant { name, args } if name == "Ok" && args.len() == 1 => args.into_iter().next().unwrap(),
        other => panic!("expected Ok([]), got {other:?}"),
    };
    assert_eq!(names, Value::List(vec![]));
}
