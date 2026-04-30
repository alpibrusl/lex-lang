//! M11 stdlib acceptance: §3.13 example B end-to-end + per-module checks.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;

fn compile(src: &str) -> lex_bytecode::Program {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        panic!("type errors: {errs:#?}");
    }
    compile_program(&stages)
}

fn run(src: &str, func: &str, args: Vec<Value>) -> Value {
    let prog = compile(src);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&prog, Box::new(handler));
    vm.call(func, args).expect("vm error")
}

#[test]
fn str_concat_split_join_round_trip() {
    let src = r#"
import "std.str" as str
fn join_split(a :: Str, b :: Str) -> Str {
  str.concat(a, b)
}
"#;
    let v = run(src, "join_split", vec![Value::Str("foo".into()), Value::Str("bar".into())]);
    assert_eq!(v, Value::Str("foobar".into()));
}

#[test]
fn str_to_int_returns_option() {
    let src = r#"
import "std.str" as str
fn parse(s :: Str) -> Option[Int] { str.to_int(s) }
"#;
    let ok = run(src, "parse", vec![Value::Str("42".into())]);
    assert_eq!(ok, Value::Variant { name: "Some".into(), args: vec![Value::Int(42)] });
    let none = run(src, "parse", vec![Value::Str("nope".into())]);
    assert_eq!(none, Value::Variant { name: "None".into(), args: vec![] });
}

#[test]
fn int_to_str_works() {
    let src = r#"
import "std.int" as int
fn show(n :: Int) -> Str { int.to_str(n) }
"#;
    assert_eq!(run(src, "show", vec![Value::Int(42)]), Value::Str("42".into()));
}

#[test]
fn list_basics() {
    let src = r#"
import "std.list" as list
fn count(xs :: List[Int]) -> Int { list.len(xs) }
"#;
    let xs = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert_eq!(run(src, "count", vec![xs]), Value::Int(3));
}

#[test]
fn list_range_and_concat() {
    let src = r#"
import "std.list" as list
fn r(lo :: Int, hi :: Int) -> List[Int] { list.range(lo, hi) }
"#;
    let v = run(src, "r", vec![Value::Int(0), Value::Int(3)]);
    assert_eq!(v, Value::List(vec![Value::Int(0), Value::Int(1), Value::Int(2)]));
}

#[test]
fn option_unwrap_or() {
    let src = r#"
import "std.option" as option
fn pick(o :: Option[Int]) -> Int { option.unwrap_or(o, 99) }
"#;
    let some = Value::Variant { name: "Some".into(), args: vec![Value::Int(5)] };
    let none = Value::Variant { name: "None".into(), args: vec![] };
    assert_eq!(run(src, "pick", vec![some]), Value::Int(5));
    assert_eq!(run(src, "pick", vec![none]), Value::Int(99));
}

#[test]
fn json_round_trip() {
    let src = r#"
import "std.json" as json
fn encode(v :: Int) -> Str { json.stringify(v) }
"#;
    assert_eq!(run(src, "encode", vec![Value::Int(7)]), Value::Str("7".into()));
}

#[test]
fn closure_captures_outer_local() {
    // Lambda captures `n` and applies it to its argument.
    let src = r#"
fn make_adder(n :: Int) -> (Int) -> Int {
  fn (x :: Int) -> Int { x + n }
}
fn driver(n :: Int, x :: Int) -> Int {
  let f := make_adder(n)
  f(x)
}
"#;
    assert_eq!(run(src, "driver", vec![Value::Int(10), Value::Int(5)]), Value::Int(15));
}

#[test]
fn result_map_applies_closure_on_ok() {
    let src = r#"
import "std.result" as result
fn double_ok(r :: Result[Int, Str]) -> Result[Int, Str] {
  result.map(r, fn (n :: Int) -> Int { n * 2 })
}
"#;
    let ok = Value::Variant { name: "Ok".into(), args: vec![Value::Int(21)] };
    assert_eq!(run(src, "double_ok", vec![ok]),
        Value::Variant { name: "Ok".into(), args: vec![Value::Int(42)] });
    let err = Value::Variant { name: "Err".into(), args: vec![Value::Str("nope".into())] };
    assert_eq!(run(src, "double_ok", vec![err.clone()]), err);
}

#[test]
fn option_map_applies_closure_on_some() {
    let src = r#"
import "std.option" as option
fn double_some(o :: Option[Int]) -> Option[Int] {
  option.map(o, fn (n :: Int) -> Int { n * 2 })
}
"#;
    let some = Value::Variant { name: "Some".into(), args: vec![Value::Int(7)] };
    assert_eq!(run(src, "double_some", vec![some]),
        Value::Variant { name: "Some".into(), args: vec![Value::Int(14)] });
    let none = Value::Variant { name: "None".into(), args: vec![] };
    assert_eq!(run(src, "double_some", vec![none.clone()]), none);
}

#[test]
fn example_b_double_input_runs_end_to_end() {
    // §3.13 example B: parse_int then result.map with a doubling lambda.
    let src = r#"
import "std.str" as str
import "std.result" as result

type ParseError = Empty | NotNumber

fn parse_int(s :: Str) -> Result[Int, ParseError] {
  if str.is_empty(s) {
    Err(Empty)
  } else {
    match str.to_int(s) {
      Some(n) => Ok(n),
      None    => Err(NotNumber),
    }
  }
}

fn double_input(s :: Str) -> Result[Int, ParseError] {
  parse_int(s) |> result.map(fn (n :: Int) -> Int { n * 2 })
}
"#;
    assert_eq!(run(src, "double_input", vec![Value::Str("21".into())]),
        Value::Variant { name: "Ok".into(), args: vec![Value::Int(42)] });
    assert_eq!(run(src, "double_input", vec![Value::Str("".into())]),
        Value::Variant { name: "Err".into(), args: vec![Value::Variant { name: "Empty".into(), args: vec![] }] });
    assert_eq!(run(src, "double_input", vec![Value::Str("xx".into())]),
        Value::Variant { name: "Err".into(), args: vec![Value::Variant { name: "NotNumber".into(), args: vec![] }] });
}
