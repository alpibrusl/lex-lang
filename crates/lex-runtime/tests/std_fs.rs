//! Integration tests for `std.fs`. Closes #99.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::Arc;

fn policy_walk_only(read_root: &std::path::Path) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["fs_walk".to_string()].into_iter().collect::<BTreeSet<_>>();
    p.allow_fs_read = vec![read_root.to_path_buf()];
    p
}

fn policy_walk_and_write(read_root: &std::path::Path, write_root: &std::path::Path) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["fs_walk".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    p.allow_fs_read = vec![read_root.to_path_buf()];
    p.allow_fs_write = vec![write_root.to_path_buf()];
    p
}

fn run(src: &str, fn_name: &str, args: Vec<Value>, policy: Policy) -> Value {
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

fn unique_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-fs-{}-{}-{}",
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
import "std.fs" as fs
import "std.list" as list

fn does_exist(path :: Str) -> [fs_walk] Bool { fs.exists(path) }

fn is_a_dir(path :: Str) -> [fs_walk] Bool { fs.is_dir(path) }

fn count_walk(path :: Str) -> [fs_walk] Int {
  match fs.walk(path) {
    Ok(paths) => list.len(paths),
    Err(_)    => 0 - 1,
  }
}

fn list_dir_count(path :: Str) -> [fs_walk] Int {
  match fs.list_dir(path) {
    Ok(paths) => list.len(paths),
    Err(_)    => 0 - 1,
  }
}

fn make_dir(path :: Str) -> [fs_write] Bool {
  match fs.mkdir_p(path) {
    Ok(_)  => true,
    Err(_) => false,
  }
}

fn stat_size(path :: Str) -> [fs_walk] Int {
  match fs.stat(path) {
    Ok(s)  => s.size,
    Err(_) => 0 - 1,
  }
}
"#;

#[test]
fn exists_returns_true_for_a_real_path() {
    let dir = unique_dir("exists_true");
    let v = run(
        SRC,
        "does_exist",
        vec![Value::Str(dir.to_string_lossy().to_string())],
        policy_walk_only(&dir),
    );
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn exists_returns_false_for_nonexistent_path() {
    let dir = unique_dir("exists_false");
    let phantom = dir.join("nope");
    let v = run(
        SRC,
        "does_exist",
        vec![Value::Str(phantom.to_string_lossy().to_string())],
        policy_walk_only(&dir),
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn is_dir_distinguishes_dir_from_file() {
    let dir = unique_dir("is_dir");
    let file = dir.join("a.txt");
    std::fs::write(&file, "hi").unwrap();

    let v = run(SRC, "is_a_dir", vec![Value::Str(dir.to_string_lossy().to_string())], policy_walk_only(&dir));
    assert_eq!(v, Value::Bool(true));

    let v = run(SRC, "is_a_dir", vec![Value::Str(file.to_string_lossy().to_string())], policy_walk_only(&dir));
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn walk_returns_recursive_path_count() {
    let dir = unique_dir("walk");
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
    // walk yields: dir, sub, a.txt, sub/b.txt → 4 entries.
    let v = run(
        SRC,
        "count_walk",
        vec![Value::Str(dir.to_string_lossy().to_string())],
        policy_walk_only(&dir),
    );
    assert_eq!(v, Value::Int(4));
}

#[test]
fn list_dir_returns_immediate_children_only() {
    let dir = unique_dir("list_dir");
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("a.txt"), "a").unwrap();
    std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
    // list_dir yields only the direct children: sub, a.txt → 2.
    let v = run(
        SRC,
        "list_dir_count",
        vec![Value::Str(dir.to_string_lossy().to_string())],
        policy_walk_only(&dir),
    );
    assert_eq!(v, Value::Int(2));
}

#[test]
fn mkdir_p_creates_nested() {
    let root = unique_dir("mkdir_root");
    let nested = root.join("a/b/c");
    let v = run(
        SRC,
        "make_dir",
        vec![Value::Str(nested.to_string_lossy().to_string())],
        policy_walk_and_write(&root, &root),
    );
    assert_eq!(v, Value::Bool(true));
    assert!(nested.exists());
}

#[test]
fn mkdir_p_outside_write_root_returns_err() {
    let allowed = unique_dir("mkdir_allowed");
    let outside = unique_dir("mkdir_outside").join("new_subdir");
    let v = run(
        SRC,
        "make_dir",
        vec![Value::Str(outside.to_string_lossy().to_string())],
        policy_walk_and_write(&allowed, &allowed),
    );
    assert_eq!(v, Value::Bool(false));
}

#[test]
fn stat_returns_size_of_file() {
    let dir = unique_dir("stat");
    let file = dir.join("payload.txt");
    std::fs::write(&file, b"abcdef").unwrap();
    let v = run(
        SRC,
        "stat_size",
        vec![Value::Str(file.to_string_lossy().to_string())],
        policy_walk_only(&dir),
    );
    assert_eq!(v, Value::Int(6));
}
