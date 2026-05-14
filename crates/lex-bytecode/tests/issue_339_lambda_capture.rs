//! Regression for #339 — lambda capture filter used to exclude
//! free vars that were *both* in the enclosing scope's locals
//! AND in the program's function-name table. When a top-level
//! user fn shadowed a parameter name in another module, lambdas
//! inside that other module would fail to capture the local
//! reference, and the lambda's body would then materialize the
//! top-level fn as a closure value instead. First field access
//! on the misresolved value panicked with
//! `GetField on non-record: Closure { ... }`.
//!
//! Fix: drop the `!function_names.contains_key(n)` clause from
//! the capture filter. If the name is in the enclosing scope's
//! locals, capture it — the local shadows the global *within*
//! this scope, exactly like the bytecode `Var` case (which
//! checks locals first) expects.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Value, Vm};
use lex_syntax::parse_source;

fn run(src: &str, fn_name: &str, args: Vec<Value>) -> Value {
    let p = parse_source(src).expect("parse");
    let stages = canonicalize_program(&p);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let prog = compile_program(&stages);
    let mut vm = Vm::new(&prog);
    vm.call(fn_name, args).unwrap_or_else(|e| panic!("call {fn_name}: {e}"))
}

const FN_NAME_SHADOWS_PARAM_IN_LAMBDA: &str = r#"
import "std.list" as list

type Field = { name :: Str, kind :: Str }
type ModelSchema = { title :: Str, fields :: List[Field] }

# Inner fn whose param `schema` collides with the top-level fn `schema`
# below. Body uses a closure that references the param — the lambda
# must capture the local, not materialize the top-level fn as a value.
# The field access `schema.title` is what triggered the original
# "GetField on non-record: Closure" panic.
fn count_escapes(schema :: ModelSchema) -> Int {
  list.fold(schema.fields, 0, fn(acc :: Int, _f :: Field) -> Int {
    match schema.title { "U" => acc + 1, _ => acc }
  })
}

# Top-level fn whose name is also the param name above.
fn schema() -> ModelSchema {
  { title: "U", fields: [{ name: "x", kind: "Str" }] }
}

fn run_it() -> Int { count_escapes(schema()) }
"#;

#[test]
fn issue_339_lambda_captures_local_shadowing_top_level_fn() {
    // Pre-fix this panicked: "GetField on non-record: Closure { ... }".
    // The lambda failed to capture `schema` (the param), so the
    // body's `schema.fields` materialized the top-level `fn schema()`
    // as a closure value, which has no `.fields`.
    assert_eq!(run(FN_NAME_SHADOWS_PARAM_IN_LAMBDA, "run_it", vec![]), Value::Int(1));
}

#[test]
fn issue_339_lambda_capture_without_collision_still_works() {
    // Sanity check: capture works the same way for names that
    // *don't* collide with top-level fns.
    let src = r#"
import "std.list" as list

fn count(xs :: List[Int]) -> Int {
  list.fold(xs, 0, fn(acc :: Int, x :: Int) -> Int { acc + x })
}

fn run_it() -> Int { count([1, 2, 3, 4]) }
"#;
    assert_eq!(run(src, "run_it", vec![]), Value::Int(10));
}

#[test]
fn issue_339_top_level_fn_referenced_in_lambda_still_resolves() {
    // Counter-test: when a lambda references a name that's *only*
    // a top-level fn (not also a local), it should resolve to the
    // top-level fn. This was the pre-#339 behavior and must
    // still work.
    let src = r#"
import "std.list" as list

fn double(x :: Int) -> Int { x * 2 }

# Lambda body references `double` — no local with that name, so it
# must resolve to the top-level fn.
fn run_it() -> List[Int] {
  list.map([1, 2, 3], fn(x :: Int) -> Int { double(x) })
}
"#;
    assert_eq!(
        run(src, "run_it", vec![]),
        Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)].into()),
    );
}
