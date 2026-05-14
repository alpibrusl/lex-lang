//! Regression tests for every lex-pydantic issue closed by PR #333:
//! #319 (inline ascription), #320 (option.unwrap_or_else),
//! #321 (list.enumerate), #322 (deep JSON type validation),
//! #323 (type-alias transparency), #324 (`_` lambda param),
//! #325 (float scientific notation), #326 (regex.is_match_str),
//! #328 (nested record-alias coercion), #329 (negative literal
//! patterns), plus the bonus #332 / #334 (Str ordering ops +
//! `list.reverse`).
//!
//! Each test is a minimal `.lex` snippet that demonstrates the
//! feature working end-to-end (parse → canonicalize → type-check
//! → bytecode → VM). The tests are intentionally compact so a
//! future breakage points at one issue rather than a wall of
//! diff.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run(src: &str, entry: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).expect("parses");
    let mut stages = canonicalize_program(&prog);
    // Use `check_and_rewrite_program` (not bare `check_program`) so
    // the `parse` → `parse_strict_typed` rewrite for #322 fires.
    if let Err(errs) = lex_types::check_and_rewrite_program(&mut stages) {
        panic!("type errors:\n{errs:#?}");
    }
    let bc = lex_bytecode::compile_program(&stages);
    let handler = DefaultHandler::new(Policy::pure());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(entry, args).unwrap_or_else(|e| panic!("call {entry}: {e:?}"))
}

fn typecheck_ok(src: &str) {
    let prog = parse_source(src).expect("parses");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type-check should pass; errors:\n{errs:#?}");
    }
}

fn typecheck_err(src: &str) {
    let prog = parse_source(src).expect("parses");
    let stages = canonicalize_program(&prog);
    match lex_types::check_program(&stages) {
        Err(_) => {}
        Ok(_) => panic!("type-check should fail"),
    }
}

// ---------------------------------------------------------------
// #319 — inline type ascription `(expr :: Type)`
// ---------------------------------------------------------------

#[test]
fn issue_319_ascription_accepts_matching_type() {
    let src = "fn r() -> Int { (42 :: Int) }\n";
    assert_eq!(run(src, "r", vec![]), Value::Int(42));
}

#[test]
fn issue_319_ascription_rejects_mismatched_type() {
    let src = "fn r() -> Int { (\"oops\" :: Int) }\n";
    typecheck_err(src);
}

#[test]
fn issue_319_nested_ascription_round_trips() {
    let src = "fn r() -> Int { ((42 :: Int) :: Int) }\n";
    assert_eq!(run(src, "r", vec![]), Value::Int(42));
}

// ---------------------------------------------------------------
// #320 — option.unwrap_or_else (lazy default thunk)
// ---------------------------------------------------------------

#[test]
fn issue_320_unwrap_or_else_some_path() {
    let src = r#"
import "std.option" as option
fn r() -> Int { option.unwrap_or_else(Some(7), fn() -> Int { 99 }) }
"#;
    assert_eq!(run(src, "r", vec![]), Value::Int(7));
}

#[test]
fn issue_320_unwrap_or_else_none_calls_thunk() {
    let src = r#"
import "std.option" as option
fn r() -> Int { option.unwrap_or_else(None, fn() -> Int { 99 }) }
"#;
    assert_eq!(run(src, "r", vec![]), Value::Int(99));
}

// ---------------------------------------------------------------
// #321 — list.enumerate (Int-indexed pairing)
// ---------------------------------------------------------------

#[test]
fn issue_321_enumerate_pairs_index_with_element() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Str]) -> List[(Int, Str)] { list.enumerate(xs) }
"#;
    let xs = Value::List(vec![
        Value::Str("a".into()),
        Value::Str("b".into()),
        Value::Str("c".into()),
    ].into());
    let out = run(src, "r", vec![xs]);
    let Value::List(items) = out else { panic!() };
    assert_eq!(items.len(), 3);
    assert_eq!(
        items[0],
        Value::Tuple(vec![Value::Int(0), Value::Str("a".into())])
    );
    assert_eq!(
        items[2],
        Value::Tuple(vec![Value::Int(2), Value::Str("c".into())])
    );
}

// ---------------------------------------------------------------
// #322 — deep JSON type validation in parse_strict_typed
// ---------------------------------------------------------------

#[test]
fn issue_322_parse_accepts_well_typed_json() {
    let src = r#"
import "std.json" as json
type User = { name :: Str, age :: Int }
fn r(s :: Str) -> Result[User, Str] { json.parse(s) }
"#;
    let out = run(
        src,
        "r",
        vec![Value::Str(r#"{"name":"alice","age":30}"#.into())],
    );
    let Value::Variant { name, .. } = out else { panic!() };
    assert_eq!(name, "Ok");
}

#[test]
fn issue_322_parse_rejects_mistyped_field() {
    let src = r#"
import "std.json" as json
type User = { name :: Str, age :: Int }
fn r(s :: Str) -> Result[User, Str] { json.parse(s) }
"#;
    let out = run(
        src,
        "r",
        vec![Value::Str(r#"{"name":"alice","age":"thirty"}"#.into())],
    );
    let Value::Variant { name, args } = out else { panic!() };
    assert_eq!(name, "Err");
    let Value::Str(msg) = &args[0] else { panic!() };
    assert!(msg.contains("expected Int"), "msg: {msg}");
}

// ---------------------------------------------------------------
// #323 — type-alias transparency for non-record aliases
// ---------------------------------------------------------------

#[test]
fn issue_323_list_alias_is_transparent() {
    // Exact repro from the issue body.
    let src = r#"
import "std.list" as list
type Error  = { path :: Str }
type Errors = List[Error]
fn single(p :: Str) -> Errors { [{ path: p }] }
"#;
    typecheck_ok(src);
}

#[test]
fn issue_323_tuple_alias_is_transparent() {
    let src = r#"
type Path = (Int, Str)
fn r() -> Path { (42, "x") }
"#;
    typecheck_ok(src);
}

#[test]
fn issue_323_option_alias_is_transparent() {
    let src = r#"
type Maybe = Option[Int]
fn r() -> Maybe { Some(5) }
"#;
    typecheck_ok(src);
}

#[test]
fn issue_323_primitive_alias_works_with_operators() {
    let src = r#"
type UserId = Int
fn add(a :: UserId, b :: UserId) -> UserId { a + b }
fn make(n :: Int) -> UserId { n }
fn r() -> Int { add(make(2), make(3)) }
"#;
    assert_eq!(run(src, "r", vec![]), Value::Int(5));
}

#[test]
fn issue_323_two_distinct_aliases_stay_nominally_distinct() {
    // Counter-test: even with #323 transparency, two record aliases
    // of identical shape are still distinct types.
    let src = r#"
type Apple = { weight :: Int }
type Box   = { weight :: Int }
fn ship(b :: Box) -> Int { b.weight }
fn make_apple() -> Apple { { weight: 5 } }
fn r() -> Int { ship(make_apple()) }
"#;
    typecheck_err(src);
}

// ---------------------------------------------------------------
// #324 — `_` as lambda parameter name
// ---------------------------------------------------------------

#[test]
fn issue_324_underscore_lambda_param_is_synthesized() {
    let src = r#"
import "std.list" as list
fn count(xs :: List[Int]) -> Int {
  list.fold(xs, 0, fn(acc :: Int, _ :: Int) -> Int { acc + 1 })
}
"#;
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into());
    assert_eq!(run(src, "count", vec![xs]), Value::Int(3));
}

#[test]
fn issue_324_nested_underscore_lambdas_get_distinct_names() {
    let src = r#"
import "std.list" as list
fn nest(xs :: List[Int], ys :: List[Int]) -> Int {
  list.fold(xs, 0, fn(acc :: Int, _ :: Int) -> Int {
    list.fold(ys, acc, fn(a2 :: Int, _ :: Int) -> Int { a2 + 1 })
  })
}
"#;
    let xs = Value::List(vec![Value::Int(0), Value::Int(0)].into());
    let ys = Value::List(vec![Value::Int(0), Value::Int(0), Value::Int(0)].into());
    assert_eq!(run(src, "nest", vec![xs, ys]), Value::Int(6));
}

// ---------------------------------------------------------------
// #325 — float scientific notation
// ---------------------------------------------------------------

#[test]
fn issue_325_float_with_decimal_and_exponent() {
    let src = "fn r() -> Float { 1.5e3 }\n";
    assert_eq!(run(src, "r", vec![]), Value::Float(1500.0));
}

#[test]
fn issue_325_float_with_only_exponent() {
    let src = "fn r() -> Float { 2e9 }\n";
    assert_eq!(run(src, "r", vec![]), Value::Float(2_000_000_000.0));
}

#[test]
fn issue_325_float_with_negative_exponent() {
    let src = "fn r() -> Float { 1.0e-3 }\n";
    assert_eq!(run(src, "r", vec![]), Value::Float(0.001));
}

// ---------------------------------------------------------------
// #326 — regex.is_match_str
// ---------------------------------------------------------------

#[test]
fn issue_326_is_match_str_substring_match() {
    let src = r#"
import "std.regex" as regex
fn r(p :: Str, s :: Str) -> Bool { regex.is_match_str(p, s) }
"#;
    assert_eq!(
        run(src, "r", vec![Value::Str("hel".into()), Value::Str("hello".into())]),
        Value::Bool(true),
    );
    assert_eq!(
        run(src, "r", vec![Value::Str("^z".into()), Value::Str("hello".into())]),
        Value::Bool(false),
    );
}

#[test]
fn issue_326_is_match_str_returns_false_on_invalid_pattern() {
    let src = r#"
import "std.regex" as regex
fn r() -> Bool { regex.is_match_str("([", "anything") }
"#;
    assert_eq!(run(src, "r", vec![]), Value::Bool(false));
}

// ---------------------------------------------------------------
// #328 — nested record-alias coercion through containers
// ---------------------------------------------------------------

#[test]
fn issue_328_record_alias_coerces_inside_result() {
    let src = r#"
type Point = { x :: Int, y :: Int }
fn r() -> Result[Point, Str] { Ok({ x: 1, y: 2 }) }
"#;
    typecheck_ok(src);
}

#[test]
fn issue_328_record_alias_coerces_inside_option() {
    let src = r#"
type Point = { x :: Int, y :: Int }
fn r() -> Option[Point] { Some({ x: 3, y: 4 }) }
"#;
    typecheck_ok(src);
}

#[test]
fn issue_328_record_alias_coerces_inside_list() {
    let src = r#"
type Point = { x :: Int, y :: Int }
fn r() -> List[Point] { [{ x: 1, y: 2 }, { x: 3, y: 4 }] }
"#;
    typecheck_ok(src);
}

// ---------------------------------------------------------------
// #329 — negative integer / float literal patterns
// ---------------------------------------------------------------

#[test]
fn issue_329_negative_int_pattern_matches() {
    let src = r#"
fn classify(n :: Int) -> Str {
  match n {
    -1 => "neg one",
    0  => "zero",
    1  => "one",
    _  => "other",
  }
}
"#;
    assert_eq!(
        run(src, "classify", vec![Value::Int(-1)]),
        Value::Str("neg one".into()),
    );
    assert_eq!(
        run(src, "classify", vec![Value::Int(2)]),
        Value::Str("other".into()),
    );
}

// ---------------------------------------------------------------
// #332 / #334 — Str ordering operators + list.reverse (bonus)
// ---------------------------------------------------------------

#[test]
fn issue_332_str_lt_compares_lexicographically() {
    let src = "fn r() -> Bool { (\"apple\" :: Str) < \"banana\" }\n";
    assert_eq!(run(src, "r", vec![]), Value::Bool(true));
}

#[test]
fn issue_334_list_reverse_inverts_order() {
    let src = r#"
import "std.list" as list
fn r(xs :: List[Int]) -> List[Int] { list.reverse(xs) }
"#;
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into());
    assert_eq!(
        run(src, "r", vec![xs]),
        Value::List(vec![Value::Int(3), Value::Int(2), Value::Int(1)].into()),
    );
}

