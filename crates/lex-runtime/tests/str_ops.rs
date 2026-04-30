//! str module: starts_with, ends_with, contains, replace, trim,
//! to_upper, to_lower, strip_prefix, strip_suffix.

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

const PRELUDE: &str = "import \"std.str\" as str\n";

fn s(v: &str) -> Value { Value::Str(v.into()) }
fn b(v: bool) -> Value { Value::Bool(v) }
fn some(v: Value) -> Value { Value::Variant { name: "Some".into(), args: vec![v] } }
fn none() -> Value { Value::Variant { name: "None".into(), args: vec![] } }

#[test]
fn starts_with() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Bool { str.starts_with(a, b) }\n";
    assert_eq!(run(src, "t", vec![s("hello world"), s("hello")]), b(true));
    assert_eq!(run(src, "t", vec![s("hello"), s("hello world")]), b(false));
    assert_eq!(run(src, "t", vec![s(""), s("")]), b(true));
    assert_eq!(run(src, "t", vec![s("abc"), s("")]), b(true));
}

#[test]
fn ends_with() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Bool { str.ends_with(a, b) }\n";
    assert_eq!(run(src, "t", vec![s("hello world"), s("world")]), b(true));
    assert_eq!(run(src, "t", vec![s("hello"), s("world")]), b(false));
    assert_eq!(run(src, "t", vec![s("hello.lex"), s(".lex")]), b(true));
}

#[test]
fn contains() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Bool { str.contains(a, b) }\n";
    assert_eq!(run(src, "t", vec![s("hello world"), s("lo wo")]), b(true));
    assert_eq!(run(src, "t", vec![s("hello"), s("xyz")]), b(false));
}

#[test]
fn replace_works() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str, c :: Str) -> Str { str.replace(a, b, c) }\n";
    assert_eq!(run(src, "t", vec![s("hello world hello"), s("hello"), s("hi")]),
               s("hi world hi"));
    assert_eq!(run(src, "t", vec![s("abc"), s("z"), s("Z")]), s("abc"));
}

#[test]
fn trim_works() {
    let src = "import \"std.str\" as str\nfn t(a :: Str) -> Str { str.trim(a) }\n";
    assert_eq!(run(src, "t", vec![s("  hello  ")]), s("hello"));
    assert_eq!(run(src, "t", vec![s("\n\thello\n")]), s("hello"));
}

#[test]
fn case_ops() {
    let upper = "import \"std.str\" as str\nfn t(a :: Str) -> Str { str.to_upper(a) }\n";
    let lower = "import \"std.str\" as str\nfn t(a :: Str) -> Str { str.to_lower(a) }\n";
    assert_eq!(run(upper, "t", vec![s("Hello World")]), s("HELLO WORLD"));
    assert_eq!(run(lower, "t", vec![s("Hello World")]), s("hello world"));
}

#[test]
fn strip_prefix() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Option[Str] { str.strip_prefix(a, b) }\n";
    assert_eq!(run(src, "t", vec![s("/weather/SF"), s("/weather/")]), some(s("SF")));
    assert_eq!(run(src, "t", vec![s("/forecast/Paris"), s("/weather/")]), none());
    assert_eq!(run(src, "t", vec![s("hello"), s("hello")]), some(s("")));
}

#[test]
fn strip_suffix() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Option[Str] { str.strip_suffix(a, b) }\n";
    assert_eq!(run(src, "t", vec![s("hello.lex"), s(".lex")]), some(s("hello")));
    assert_eq!(run(src, "t", vec![s("hello.txt"), s(".lex")]), none());
}

#[test]
fn weather_app_still_typechecks_after_simplification() {
    // Sanity: the simplified weather app uses str.strip_prefix and
    // remains valid. We don't run net.serve here (would block); just
    // verify it parses + typechecks.
    let src = include_str!("../../../examples/weather_app.lex");
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    let _ = PRELUDE; // suppress unused warning
    lex_types::check_program(&stages).expect("typecheck");
}
