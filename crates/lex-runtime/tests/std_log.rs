//! Integration tests for `std.log`. Closes #103.
//!
//! Tests redirect the global sink to a tempfile, exercise the emit
//! ops at each level, and assert the file contents. Each test uses a
//! fresh sink path; the global sink is process-wide, so tests use a
//! mutex to serialize.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

fn test_lock() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn policy_for_log(write_root: &std::path::Path) -> Policy {
    let mut p = Policy::pure();
    p.allow_effects = ["log".to_string(), "io".to_string(), "fs_write".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
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

fn unique_log_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lex-log-{}-{}-{}",
        std::process::id(),
        name,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("log.txt")
}

const SRC: &str = r#"
import "std.log" as log

fn setup_text(path :: Str) -> [io, fs_write] Bool {
  match log.set_sink(path) {
    Ok(_) => match log.set_format("text") {
      Ok(_) => match log.set_level("debug") {
        Ok(_)  => true,
        Err(_) => false,
      },
      Err(_) => false,
    },
    Err(_) => false,
  }
}

fn setup_json(path :: Str) -> [io, fs_write] Bool {
  match log.set_sink(path) {
    Ok(_) => match log.set_format("json") {
      Ok(_) => match log.set_level("info") {
        Ok(_)  => true,
        Err(_) => false,
      },
      Err(_) => false,
    },
    Err(_) => false,
  }
}

fn setup_warn_threshold(path :: Str) -> [io, fs_write] Bool {
  match log.set_sink(path) {
    Ok(_) => match log.set_format("text") {
      Ok(_) => match log.set_level("warn") {
        Ok(_)  => true,
        Err(_) => false,
      },
      Err(_) => false,
    },
    Err(_) => false,
  }
}

fn emit_each_level() -> [log] Nil {
  log.debug("d-msg")
  log.info("i-msg")
  log.warn("w-msg")
  log.error("e-msg")
}

fn emit_one_info() -> [log] Nil { log.info("hello, world") }
"#;

#[test]
fn text_format_includes_level_and_message() {
    let _g = test_lock().lock().unwrap();
    let path = unique_log_path("text_levels");
    let policy = policy_for_log(path.parent().unwrap());

    run(SRC, "setup_text",
        vec![Value::Str(path.to_string_lossy().to_string())], policy.clone());
    run(SRC, "emit_each_level", vec![], policy);

    let contents = std::fs::read_to_string(&path).expect("read log");
    assert!(contents.contains("debug: d-msg"), "got: {contents}");
    assert!(contents.contains("info: i-msg"), "got: {contents}");
    assert!(contents.contains("warn: w-msg"), "got: {contents}");
    assert!(contents.contains("error: e-msg"), "got: {contents}");
}

#[test]
fn json_format_emits_one_object_per_line() {
    let _g = test_lock().lock().unwrap();
    let path = unique_log_path("json_format");
    let policy = policy_for_log(path.parent().unwrap());

    run(SRC, "setup_json",
        vec![Value::Str(path.to_string_lossy().to_string())], policy.clone());
    run(SRC, "emit_one_info", vec![], policy);

    let contents = std::fs::read_to_string(&path).expect("read log");
    let parsed: serde_json::Value = serde_json::from_str(contents.trim())
        .unwrap_or_else(|e| panic!("expected JSON line, got `{contents}`: {e}"));
    assert_eq!(parsed["level"], "info");
    assert_eq!(parsed["msg"], "hello, world");
    assert!(parsed["ts"].is_string(), "missing ts: {parsed}");
}

#[test]
fn level_threshold_drops_lower_levels() {
    let _g = test_lock().lock().unwrap();
    let path = unique_log_path("level_threshold");
    let policy = policy_for_log(path.parent().unwrap());

    run(SRC, "setup_warn_threshold",
        vec![Value::Str(path.to_string_lossy().to_string())], policy.clone());
    run(SRC, "emit_each_level", vec![], policy);

    let contents = std::fs::read_to_string(&path).expect("read log");
    // debug + info dropped; warn + error survive.
    assert!(!contents.contains("d-msg"), "debug should be filtered: {contents}");
    assert!(!contents.contains("i-msg"), "info should be filtered: {contents}");
    assert!(contents.contains("w-msg"), "warn should pass: {contents}");
    assert!(contents.contains("e-msg"), "error should pass: {contents}");
}

#[test]
fn invalid_level_returns_err() {
    let _g = test_lock().lock().unwrap();
    let path = unique_log_path("bad_level");
    let policy = policy_for_log(path.parent().unwrap());

    let src = r#"
import "std.log" as log
fn try_bad_level() -> [io] Bool {
  match log.set_level("trace") {
    Ok(_)  => false,
    Err(_) => true,
  }
}
"#;
    let v = run(src, "try_bad_level", vec![], policy);
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn set_sink_outside_write_root_returns_err() {
    let _g = test_lock().lock().unwrap();
    let allowed = std::env::temp_dir().join(format!(
        "lex-log-allowed-{}", std::process::id()));
    std::fs::create_dir_all(&allowed).unwrap();
    let outside = unique_log_path("outside_root");

    let src = r#"
import "std.log" as log
fn setup(path :: Str) -> [io, fs_write] Bool {
  match log.set_sink(path) {
    Ok(_)  => true,
    Err(_) => false,
  }
}
"#;
    let policy = policy_for_log(&allowed);
    let v = run(src, "setup",
        vec![Value::Str(outside.to_string_lossy().to_string())], policy);
    assert_eq!(v, Value::Bool(false));
}
