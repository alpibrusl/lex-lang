//! Closures-as-values in record fields (#169). The fix is one
//! arm in the bytecode compiler's `Var` case — when a function
//! name is used in a value position (record-field initializer,
//! HOF callback arg, etc.), materialize it as
//! `Value::Closure { fn_id, captures: vec![] }` instead of
//! panicking with `unknown local`.

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

const RECORD_HOLDS_FN_NAME: &str = r#"
type Test = { name :: Str, run :: () -> Result[Str, Str] }

fn pass() -> Result[Str, Str] { Ok("ok") }

fn run_one(t :: Test) -> Result[Str, Str] { t.run() }

fn entry() -> Str {
  let t :: Test := { name: "smoke", run: pass }
  match run_one(t) {
    Ok(s)  => s,
    Err(e) => e,
  }
}
"#;

#[test]
fn record_field_holds_named_fn_and_calls_through() {
    let v = run(RECORD_HOLDS_FN_NAME, "entry", vec![]);
    assert_eq!(v, Value::Str("ok".into()));
}

const RECORD_HOLDS_LAMBDA: &str = r#"
type Action = { id :: Int, do :: (Int) -> Int }

fn entry() -> Int {
  let a :: Action := { id: 1, do: fn (x :: Int) -> Int { x * 2 } }
  a.do(21)
}
"#;

#[test]
fn record_field_holds_lambda_and_calls_through() {
    // Lambdas worked before #169 (they always materialized as
    // closures). Pin that the named-fn fix didn't regress them.
    let v = run(RECORD_HOLDS_LAMBDA, "entry", vec![]);
    assert_eq!(v, Value::Int(42));
}

const SHORT_CIRCUITING_SUITE: &str = r#"
import "std.list" as list

type Test = { name :: Str, run :: () -> Result[Str, Str] }

fn pass_a() -> Result[Str, Str] { Ok("a") }
fn fail_b() -> Result[Str, Str] { Err("b broke") }
fn pass_c() -> Result[Str, Str] { Ok("c") }

# Stops at the first Err — the rubric short-circuit pattern from #169.
# Tests are deferred (() -> ...), so only those before-and-including
# the failure are evaluated.
fn run_until_first_failure(suite :: List[Test]) -> Result[Str, Str] {
  list.fold(suite, Ok("none"), fn (acc :: Result[Str, Str], t :: Test) -> Result[Str, Str] {
    match acc {
      Ok(_)  => t.run(),
      Err(e) => Err(e),
    }
  })
}

fn entry() -> Str {
  let suite :: List[Test] := [
    { name: "a", run: pass_a },
    { name: "b", run: fail_b },
    { name: "c", run: pass_c },
  ]
  match run_until_first_failure(suite) {
    Ok(_)  => "all-passed",
    Err(e) => e,
  }
}
"#;

#[test]
fn rubric_short_circuit_suite_stops_at_first_failure() {
    let v = run(SHORT_CIRCUITING_SUITE, "entry", vec![]);
    assert_eq!(v, Value::Str("b broke".into()),
        "fold should bail at fail_b without going on to pass_c");
}

const FN_AS_HOF_ARG: &str = r#"
import "std.list" as list

fn double(x :: Int) -> Int { x * 2 }

fn entry() -> List[Int] { list.map([1, 2, 3], double) }
"#;

#[test]
fn named_fn_passed_directly_as_hof_arg() {
    // The same gap covers passing a named fn to list.map without
    // wrapping in a lambda. Pre-fix the user had to write
    // `fn (x) -> Int { double(x) }`; now `double` works directly.
    let v = run(FN_AS_HOF_ARG, "entry", vec![]);
    match v {
        Value::List(items) => {
            assert_eq!(items, vec![Value::Int(2), Value::Int(4), Value::Int(6)]);
        }
        other => panic!("expected List, got {other:?}"),
    }
}
