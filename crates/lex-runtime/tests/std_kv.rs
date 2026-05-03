//! Integration tests for `std.kv`. Closes #100.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn policy_with_kv(write_root: &std::path::Path) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["kv".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    p.allow_fs_write = vec![write_root.to_path_buf()];
    p
}

fn run_with_policy(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(policy).with_program(Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

fn unique_db_path(name: &str) -> std::path::PathBuf {
    // Each test gets a fresh dir so tests don't share state and the
    // sled lockfile doesn't collide.
    let dir = std::env::temp_dir().join(format!(
        "lex-kv-{}-{}-{}",
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

const SRC: &str = r#"
import "std.kv" as kv

# Round-trip: open, put, get, return Some payload as Bytes.
fn put_then_get(path :: Str, key :: Str, val :: Bytes) -> [kv, fs_write] Option[Bytes] {
  match kv.open(path) {
    Ok(db) => match kv.put(db, key, val) {
      Ok(_)  => kv.get(db, key),
      Err(_) => None,
    },
    Err(_) => None,
  }
}

fn get_missing(path :: Str, key :: Str) -> [kv, fs_write] Option[Bytes] {
  match kv.open(path) {
    Ok(db) => kv.get(db, key),
    Err(_) => None,
  }
}

fn put_then_contains(path :: Str, key :: Str, val :: Bytes) -> [kv, fs_write] Bool {
  match kv.open(path) {
    Ok(db) => match kv.put(db, key, val) {
      Ok(_)  => kv.contains(db, key),
      Err(_) => false,
    },
    Err(_) => false,
  }
}

fn delete_then_contains(path :: Str, key :: Str, val :: Bytes) -> [kv, fs_write] Bool {
  match kv.open(path) {
    Ok(db) => match kv.put(db, key, val) {
      Ok(_)  => match kv.delete(db, key) {
        Ok(_)  => kv.contains(db, key),
        Err(_) => true,
      },
      Err(_) => true,
    },
    Err(_) => true,
  }
}

fn list_prefix_keys(path :: Str) -> [kv, fs_write] List[Str] {
  match kv.open(path) {
    Ok(db) => match kv.put(db, "user:1", b"a") {
      Ok(_) => match kv.put(db, "user:2", b"b") {
        Ok(_) => match kv.put(db, "session:x", b"c") {
          Ok(_) => kv.list_prefix(db, "user:"),
          Err(_) => [],
        },
        Err(_) => [],
      },
      Err(_) => [],
    },
    Err(_) => [],
  }
}
"#;

#[test]
fn put_then_get_round_trips() {
    let path = unique_db_path("put_then_get");
    let policy = policy_with_kv(&path);
    let v = run_with_policy(
        SRC,
        "put_then_get",
        vec![
            Value::Str(path.to_string_lossy().to_string()),
            Value::Str("k1".into()),
            Value::Bytes(b"hello".to_vec()),
        ],
        policy,
    );
    assert_eq!(
        v,
        Value::Variant {
            name: "Some".into(),
            args: vec![Value::Bytes(b"hello".to_vec())],
        },
    );
}

#[test]
fn get_missing_returns_none() {
    let path = unique_db_path("get_missing");
    let policy = policy_with_kv(&path);
    let v = run_with_policy(
        SRC,
        "get_missing",
        vec![
            Value::Str(path.to_string_lossy().to_string()),
            Value::Str("never_set".into()),
        ],
        policy,
    );
    assert_eq!(v, Value::Variant { name: "None".into(), args: vec![] });
}

#[test]
fn contains_after_put() {
    let path = unique_db_path("contains_after_put");
    let policy = policy_with_kv(&path);
    let v = run_with_policy(
        SRC,
        "put_then_contains",
        vec![
            Value::Str(path.to_string_lossy().to_string()),
            Value::Str("a".into()),
            Value::Bytes(b"x".to_vec()),
        ],
        policy,
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn delete_removes_key() {
    let path = unique_db_path("delete");
    let policy = policy_with_kv(&path);
    let v = run_with_policy(
        SRC,
        "delete_then_contains",
        vec![
            Value::Str(path.to_string_lossy().to_string()),
            Value::Str("a".into()),
            Value::Bytes(b"x".to_vec()),
        ],
        policy,
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn list_prefix_returns_only_matching_keys() {
    let path = unique_db_path("list_prefix");
    let policy = policy_with_kv(&path);
    let v = run_with_policy(SRC, "list_prefix_keys", vec![
        Value::Str(path.to_string_lossy().to_string()),
    ], policy);

    let keys: Vec<String> = match v {
        Value::List(items) => items
            .into_iter()
            .map(|i| match i {
                Value::Str(s) => s,
                other => panic!("expected Str in list, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List, got {other:?}"),
    };
    assert_eq!(
        keys.iter().filter(|k| k.starts_with("user:")).count(),
        2
    );
    assert!(!keys.iter().any(|k| k.starts_with("session:")));
}

#[test]
fn open_outside_fs_write_root_returns_err() {
    // The static policy walk only checks effect kinds, not paths;
    // path scoping is enforced at the runtime dispatch site.
    let allowed = unique_db_path("open_scope_allowed");
    let outside = unique_db_path("open_scope_outside");
    let mut policy = policy_with_kv(&allowed);
    // Re-scope to *only* `allowed` (overwriting the default from the
    // helper, which adds `outside` implicitly through the test setup).
    policy.allow_fs_write = vec![allowed.clone()];

    let v = run_with_policy(
        SRC,
        "put_then_get",
        vec![
            Value::Str(outside.to_string_lossy().to_string()),
            Value::Str("k".into()),
            Value::Bytes(b"v".to_vec()),
        ],
        policy,
    );
    // The outer match in put_then_get returns None on the open Err.
    assert_eq!(v, Value::Variant { name: "None".into(), args: vec![] });
}
