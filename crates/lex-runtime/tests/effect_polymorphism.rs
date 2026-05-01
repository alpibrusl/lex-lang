//! Effect polymorphism on stdlib HOFs: list.map / list.filter /
//! list.fold / option.map / result.map / result.and_then / result.map_err
//! all carry an effect-row variable so an effectful closure
//! propagates its effects to the call site.
//!
//! Pre-effect-polymorphism: passing a `(Str) -> [net] Str` closure
//! to list.map was a type error because the signature pinned the
//! callback as `(Str) -> Str` (pure). With `EffectSet::open_var`
//! the row variable unifies with the closure's actual effects and
//! the propagation lands on `list.map`'s return type.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).expect("vm")
}

fn type_check(src: &str) -> Result<(), Vec<lex_types::TypeError>> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    lex_types::check_program(&stages).map(|_| ())
}

// -- list.map -----------------------------------------------------

#[test]
fn list_map_with_effectful_closure_type_checks() {
    // The closure body uses time.now (effect [time]); list.map must
    // accept it and the surrounding fn must declare [time].
    let src = r#"
import "std.list" as list
import "std.time" as time
fn timestamp_each(xs :: List[Int]) -> [time] List[Int] {
  list.map(xs, fn (x :: Int) -> [time] Int { x + time.now() })
}
"#;
    type_check(src).expect("type-check should accept effectful closure");
}

#[test]
fn list_map_propagated_effect_must_be_declared_on_caller() {
    // Same body, but the surrounding fn forgets to declare [time].
    // Effect-polymorphism propagated [time] via the open row var,
    // so the caller's signature is now under-declared → type error.
    let src = r#"
import "std.list" as list
import "std.time" as time
fn timestamp_each(xs :: List[Int]) -> List[Int] {
  list.map(xs, fn (x :: Int) -> [time] Int { x + time.now() })
}
"#;
    let errs = type_check(src).expect_err("expected effect leak");
    assert!(errs.iter().any(|e| matches!(e,
        lex_types::TypeError::EffectNotDeclared { effect, .. } if effect == "time")),
        "expected EffectNotDeclared(time); got {errs:#?}");
}

#[test]
fn list_map_with_pure_closure_still_works() {
    // Existing behavior: pure closures bind the effect var to {}.
    let src = r#"
import "std.list" as list
fn doubled(xs :: List[Int]) -> List[Int] {
  list.map(xs, fn (n :: Int) -> Int { n * 2 })
}
"#;
    let r = run(src, "doubled",
        vec![Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])]);
    assert_eq!(r, Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)]));
}

// -- list.filter --------------------------------------------------

#[test]
fn list_filter_with_effectful_predicate_type_checks() {
    let src = r#"
import "std.list" as list
import "std.time" as time
fn keep_recent(xs :: List[Int]) -> [time] List[Int] {
  list.filter(xs, fn (x :: Int) -> [time] Bool { x > time.now() })
}
"#;
    type_check(src).expect("filter with [time] predicate should work");
}

// -- list.fold ----------------------------------------------------

#[test]
fn list_fold_with_effectful_combiner_type_checks() {
    let src = r#"
import "std.list" as list
import "std.time" as time
fn folded(xs :: List[Int]) -> [time] Int {
  list.fold(xs, 0, fn (acc :: Int, x :: Int) -> [time] Int {
    acc + x + time.now()
  })
}
"#;
    type_check(src).expect("fold with [time] combiner should work");
}

// -- result.map / and_then ---------------------------------------

#[test]
fn result_map_with_effectful_closure_type_checks() {
    let src = r#"
import "std.result" as result
import "std.time" as time
fn stamp(r :: Result[Int, Str]) -> [time] Result[Int, Str] {
  result.map(r, fn (n :: Int) -> [time] Int { n + time.now() })
}
"#;
    type_check(src).expect("result.map with [time] closure should work");
}

#[test]
fn result_and_then_with_effectful_closure_type_checks() {
    let src = r#"
import "std.result" as result
import "std.time" as time
fn chain(r :: Result[Int, Str]) -> [time] Result[Int, Str] {
  result.and_then(r, fn (n :: Int) -> [time] Result[Int, Str] {
    Ok(n + time.now())
  })
}
"#;
    type_check(src).expect("result.and_then with [time] closure should work");
}

// -- option.map --------------------------------------------------

#[test]
fn option_map_with_effectful_closure_type_checks() {
    let src = r#"
import "std.option" as option
import "std.time" as time
fn stamp_opt(o :: Option[Int]) -> [time] Option[Int] {
  option.map(o, fn (n :: Int) -> [time] Int { n + time.now() })
}
"#;
    type_check(src).expect("option.map with [time] closure should work");
}

// -- runtime: pure path + effect-propagated path both execute ---

#[test]
fn effectful_list_map_runs_end_to_end() {
    // Use rand.int_in (deterministic stub: midpoint of [lo, hi]) so
    // we can assert exact output. Closure has [rand]; the surrounding
    // fn declares [rand]; runtime runs under permissive policy.
    let src = r#"
import "std.list" as list
import "std.rand" as rand
fn midpoints(xs :: List[Int]) -> [rand] List[Int] {
  list.map(xs, fn (hi :: Int) -> [rand] Int { rand.int_in(0, hi) })
}
"#;
    // rand.int_in(0, 10) → 5; rand.int_in(0, 100) → 50; rand.int_in(0, 6) → 3
    let r = run(src, "midpoints",
        vec![Value::List(vec![Value::Int(10), Value::Int(100), Value::Int(6)])]);
    assert_eq!(r, Value::List(vec![Value::Int(5), Value::Int(50), Value::Int(3)]));
}
