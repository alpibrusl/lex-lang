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

// #440 — str.cmp three-way comparator.
//
// Boolean comparisons (`<`, `<=`, `>`, `>=`) already work on Str via the
// VM's `bin_ord` path; the cmp(-1/0/1) shape is the new capability so
// downstream code can pass it as a sort-by closure value once
// `list.sort_by` lands.
#[test]
fn cmp_returns_neg_one_zero_one() {
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Int { str.cmp(a, b) }\n";
    let i = |n: i64| Value::Int(n);
    assert_eq!(run(src, "t", vec![s("a"), s("b")]), i(-1));
    assert_eq!(run(src, "t", vec![s("b"), s("a")]), i(1));
    assert_eq!(run(src, "t", vec![s("abc"), s("abc")]), i(0));
    // Length differences resolve before bytes run out.
    assert_eq!(run(src, "t", vec![s("ab"), s("abc")]), i(-1));
    assert_eq!(run(src, "t", vec![s("abc"), s("ab")]), i(1));
    // Empty string sorts below everything else.
    assert_eq!(run(src, "t", vec![s(""), s("a")]), i(-1));
    assert_eq!(run(src, "t", vec![s(""), s("")]), i(0));
}

#[test]
fn cmp_iso_8601_datetime_is_byte_order() {
    // The OCPI date-range use case from the issue: ISO 8601 UTC
    // strings sort lexicographically.
    let src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Int { str.cmp(a, b) }\n";
    let i = |n: i64| Value::Int(n);
    assert_eq!(
        run(src, "t", vec![s("2026-05-15T10:00:00Z"), s("2026-05-15T10:00:01Z")]),
        i(-1),
    );
    assert_eq!(
        run(src, "t", vec![s("2026-05-16T00:00:00Z"), s("2026-05-15T23:59:59Z")]),
        i(1),
    );
}

#[test]
fn cmp_total_order_sign_matches_lt_operator() {
    // For all pairs the type checker accepts, the sign of str.cmp(a, b)
    // must agree with the boolean operator: cmp < 0 ⇔ a < b. Anchors
    // the cmp behaviour to the existing operator semantics so callers
    // can mix the two without surprises.
    let cmp_src = "import \"std.str\" as str\nfn t(a :: Str, b :: Str) -> Int { str.cmp(a, b) }\n";
    let lt_src  = "fn t(a :: Str, b :: Str) -> Bool { a < b }\n";
    let pairs: &[(&str, &str)] = &[
        ("alpha", "beta"),
        ("beta",  "alpha"),
        ("",      "x"),
        ("x",     ""),
        ("foo",   "foo"),
        ("foo",   "foobar"),
    ];
    for (a, b) in pairs {
        let cmp = match run(cmp_src, "t", vec![s(a), s(b)]) {
            Value::Int(n) => n,
            other => panic!("cmp returned {other:?}"),
        };
        let lt = run(lt_src, "t", vec![s(a), s(b)]);
        assert_eq!(lt, Value::Bool(cmp < 0),
            "cmp({a:?}, {b:?}) = {cmp} but `a < b` = {lt:?}");
    }
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
