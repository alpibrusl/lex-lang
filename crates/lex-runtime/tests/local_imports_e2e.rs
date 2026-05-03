//! End-to-end: load a multi-file project, type-check, compile, run a
//! function. Exercises the loader + canonicalize + check + bytecode +
//! VM pipeline against the issue #78 use case.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::load_program;

fn write(dir: &std::path::Path, name: &str, src: &str) {
    std::fs::write(dir.join(name), src).unwrap();
}

fn run(entry: &std::path::Path, fn_name: &str, args: Vec<Value>) -> Value {
    let prog = load_program(entry).expect("load");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = std::sync::Arc::new(compile_program(&stages));
    let handler = DefaultHandler::new(Policy::pure()).with_program(std::sync::Arc::clone(&bc));
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(fn_name, args).expect("run")
}

#[test]
fn two_file_project_runs() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "helpers.lex",
        r#"fn double(x :: Int) -> Int { x + x }
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./helpers" as h
fn main(x :: Int) -> Int { h.double(x) + 1 }
"#,
    );

    let v = run(&dir.path().join("main.lex"), "main", vec![Value::Int(20)]);
    assert_eq!(v, Value::Int(41));
}

#[test]
fn imported_type_used_in_signature_runs() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "models.lex",
        r#"type Status = Healthy | Sick
fn label(s :: Status) -> Str {
  match s {
    Healthy => "ok",
    Sick    => "nope",
  }
}
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./models" as m
fn describe(s :: m.Status) -> Str { m.label(s) }
"#,
    );

    let v = run(
        &dir.path().join("main.lex"),
        "describe",
        vec![Value::Variant {
            name: "Healthy".into(),
            args: vec![],
        }],
    );
    assert_eq!(v, Value::Str("ok".into()));
}

#[test]
fn transitive_imports_run() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "c.lex", "fn z(x :: Int) -> Int { x * 2 }\n");
    write(
        dir.path(),
        "b.lex",
        r#"import "./c" as c
fn y(x :: Int) -> Int { c.z(x) + 1 }
"#,
    );
    write(
        dir.path(),
        "a.lex",
        r#"import "./b" as b
fn run(x :: Int) -> Int { b.y(x) }
"#,
    );

    let v = run(&dir.path().join("a.lex"), "run", vec![Value::Int(5)]);
    assert_eq!(v, Value::Int(11));
}

#[test]
fn shadowed_let_binding_works_at_runtime() {
    // Regression for the mangler's shadow-tracking.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "h.lex",
        r#"fn inner(x :: Int) -> Int { x + 1000 }
fn caller(x :: Int) -> Int {
  let inner := x + 7
  inner
}
"#,
    );
    write(
        dir.path(),
        "main.lex",
        r#"import "./h" as h
fn run(x :: Int) -> Int { h.caller(x) }
"#,
    );

    let v = run(&dir.path().join("main.lex"), "run", vec![Value::Int(3)]);
    assert_eq!(v, Value::Int(10), "let-bound `inner` should win over top-level `inner`");
}
