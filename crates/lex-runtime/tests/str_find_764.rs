//! #764: character-level scans written in Lex were quadratic.
//!
//! Two causes, both in the runtime: every `std.str` builtin copied
//! its whole string argument before looking at it (so `str.char_at`
//! on a 300 KB document copied 300 KB per character), and
//! `str.slice` resolved codepoint indices from the start of the
//! string on every call. The first is gone; the second now resolves
//! forward scans from the previous position. `str.find` and
//! `str.find_any` let a scanner jump to the next delimiter in one
//! call. Indices are codepoint positions, matching `str.slice`.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, Value};
use lex_runtime::{DefaultHandler, Policy};
use lex_syntax::parse_source;
use std::time::{Duration, Instant};

fn run(src: &str, func: &str, args: Vec<Value>) -> Result<Value, String> {
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    if let Err(errs) = lex_types::check_program(&stages) {
        return Err(format!("type errors: {errs:#?}"));
    }
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(func, args).map_err(|e| format!("{e}"))
}

fn some(v: Value) -> Value {
    Value::Variant { name: "Some".into(), args: vec![v] }
}

fn none() -> Value {
    Value::Variant { name: "None".into(), args: vec![] }
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

const SRC: &str = r#"
import "std.str" as str
import "std.list" as list

fn find(s :: Str, n :: Str, from :: Int) -> Option[Int] { str.find(s, n, from) }
fn is_ascii(s :: Str) -> Bool { str.is_ascii(s) }
fn find_any(s :: Str, set :: Str, from :: Int) -> Option[Int] { str.find_any(s, set, from) }
fn slice(s :: Str, lo :: Int, hi :: Int) -> Str { str.slice(s, lo, hi) }

# Forward then backward on one string: the cursor must not be
# reused for an index before the one it remembers.
fn back(s :: Str) -> Str {
  str.concat(str.slice(s, 7, 8), str.slice(s, 1, 2))
}

# Two strings of equal byte length, interleaved: the cursor is
# keyed on the string, not on its length.
fn pair(a :: Str, b :: Str) -> Str {
  let x := str.slice(a, 4, 5)
  let y := str.slice(b, 4, 5)
  str.concat(str.concat(x, y), str.concat(str.slice(a, 1, 2), str.slice(b, 1, 2)))
}

# The scanner shape from lex-schema's JSON parser: one slice per
# position, front to back.
fn walk(s :: Str) -> Int {
  list.fold(list.range(0, str.len(s)), 0, fn (acc :: Int, i :: Int) -> Int {
    acc + str.len(str.slice(s, i, i + 1))
  })
}

# A pure helper over the document, as lex-schema's parser writes it.
# `peek` is memoized once it hits (the same position is read twice
# up front), so without a bound on the key cost every later call
# would hash the whole document.
fn peek(s :: Str, i :: Int) -> Str { str.char_at(s, i) }

fn walk_peek(s :: Str) -> Int {
  let warm := str.concat(peek(s, 0), peek(s, 0))
  list.fold(list.range(0, str.len(s)), str.len(warm) - 2, fn (acc :: Int, i :: Int) -> Int {
    acc + str.len(peek(s, i))
  })
}

# Delimiter hopping with `find`: the input is "a," repeated, so the
# find from 2*i lands at 2*i + 1 and the distances sum to n.
fn hops(s :: Str, n :: Int) -> Int {
  list.fold(list.range(0, n), 0, fn (acc :: Int, i :: Int) -> Int {
    match str.find(s, ",", i * 2) {
      Some(j) => acc + (j - i * 2),
      None => acc,
    }
  })
}
"#;

#[test]
fn find_returns_first_index_at_or_after_from() {
    let src = "a,b,,c";
    assert_eq!(run(SRC, "find", vec![s(src), s(","), Value::Int(0)]).unwrap(), some(Value::Int(1)));
    assert_eq!(run(SRC, "find", vec![s(src), s(","), Value::Int(2)]).unwrap(), some(Value::Int(3)));
    assert_eq!(run(SRC, "find", vec![s(src), s(","), Value::Int(4)]).unwrap(), some(Value::Int(4)));
    assert_eq!(run(SRC, "find", vec![s(src), s(","), Value::Int(5)]).unwrap(), none());
    assert_eq!(run(SRC, "find", vec![s(src), s(","), Value::Int(100)]).unwrap(), none());
    assert_eq!(run(SRC, "find", vec![s(src), s(","), Value::Int(-3)]).unwrap(), some(Value::Int(1)));
    assert_eq!(run(SRC, "find", vec![s(src), s("b,,"), Value::Int(0)]).unwrap(), some(Value::Int(2)));
    assert_eq!(run(SRC, "find", vec![s(src), s("zz"), Value::Int(0)]).unwrap(), none());
    // An empty needle matches at `from`, including at the end.
    assert_eq!(run(SRC, "find", vec![s(src), s(""), Value::Int(2)]).unwrap(), some(Value::Int(2)));
    assert_eq!(run(SRC, "find", vec![s(src), s(""), Value::Int(6)]).unwrap(), some(Value::Int(6)));
    assert_eq!(run(SRC, "find", vec![s(""), s("x"), Value::Int(0)]).unwrap(), none());
}

#[test]
fn is_ascii_is_true_only_for_all_ascii_input() {
    assert_eq!(run(SRC, "is_ascii", vec![s("")]).unwrap(), Value::Bool(true));
    assert_eq!(run(SRC, "is_ascii", vec![s("plain ascii, \t\n 0x7f: \x7f")]).unwrap(), Value::Bool(true));
    assert_eq!(run(SRC, "is_ascii", vec![s("héllo")]).unwrap(), Value::Bool(false));
    assert_eq!(run(SRC, "is_ascii", vec![s("✓")]).unwrap(), Value::Bool(false));
    let big = format!("{}é", "a".repeat(100_000));
    assert_eq!(run(SRC, "is_ascii", vec![s(&big)]).unwrap(), Value::Bool(false));
}

#[test]
fn find_indices_are_codepoints_and_compose_with_slice() {
    let src = "héllo wörld";
    assert_eq!(run(SRC, "find", vec![s(src), s("wörld"), Value::Int(0)]).unwrap(), some(Value::Int(6)));
    assert_eq!(run(SRC, "find", vec![s(src), s("l"), Value::Int(3)]).unwrap(), some(Value::Int(3)));
    assert_eq!(run(SRC, "find", vec![s(src), s("ö"), Value::Int(0)]).unwrap(), some(Value::Int(7)));
    assert_eq!(run(SRC, "slice", vec![s(src), Value::Int(7), Value::Int(8)]).unwrap(), s("ö"));
}

#[test]
fn find_any_locates_the_next_char_from_a_set() {
    let src = "abc\"def\\g";
    let set = "\"\\";
    assert_eq!(run(SRC, "find_any", vec![s(src), s(set), Value::Int(0)]).unwrap(), some(Value::Int(3)));
    assert_eq!(run(SRC, "find_any", vec![s(src), s(set), Value::Int(3)]).unwrap(), some(Value::Int(3)));
    assert_eq!(run(SRC, "find_any", vec![s(src), s(set), Value::Int(4)]).unwrap(), some(Value::Int(7)));
    assert_eq!(run(SRC, "find_any", vec![s(src), s(set), Value::Int(8)]).unwrap(), none());
    assert_eq!(run(SRC, "find_any", vec![s(src), s(""), Value::Int(0)]).unwrap(), none());
    assert_eq!(run(SRC, "find_any", vec![s("héllo"), s("lo"), Value::Int(0)]).unwrap(), some(Value::Int(2)));
}

#[test]
fn cursor_does_not_serve_backward_or_cross_string_lookups() {
    // Long enough to live on the heap, where the cursor applies.
    let a = format!("héllo{}", "x".repeat(40));
    let b = format!("wördz{}", "y".repeat(40));
    assert_eq!(run(SRC, "back", vec![s(&a)]).unwrap(), s("xé"));
    assert_eq!(run(SRC, "pair", vec![s(&a), s(&b)]).unwrap(), s("ozéö"));
    // A fresh string of the same length after the cursor was set.
    let c = format!("wörld{}", "z".repeat(40));
    assert_eq!(run(SRC, "slice", vec![s(&c), Value::Int(1), Value::Int(2)]).unwrap(), s("ö"));
}

#[test]
fn cursor_resolves_a_short_backward_hop_across_multibyte_chars() {
    // find lands the cursor on the delimiter; the chunk slice then
    // starts a few codepoints behind it, across non-ASCII chars.
    let src = format!("{}é✓ö,tail{}", "a".repeat(40), "b".repeat(40));
    // codepoints: 0..40 'a', 40 'é', 41 '✓', 42 'ö', 43 ','
    assert_eq!(run(SRC, "find", vec![s(&src), s(","), Value::Int(0)]).unwrap(), some(Value::Int(43)));
    assert_eq!(run(SRC, "slice", vec![s(&src), Value::Int(40), Value::Int(43)]).unwrap(), s("é✓ö"));
    assert_eq!(run(SRC, "slice", vec![s(&src), Value::Int(41), Value::Int(42)]).unwrap(), s("✓"));
    assert_eq!(run(SRC, "slice", vec![s(&src), Value::Int(39), Value::Int(41)]).unwrap(), s("aé"));
}

#[test]
fn slice_walk_is_linear() {
    let n = 200_000;
    let src = "a".repeat(n);
    let t = Instant::now();
    let v = run(SRC, "walk", vec![s(&src)]).unwrap();
    assert_eq!(v, Value::Int(n as i64));
    // Quadratic resolution took minutes at this size in a debug build.
    assert!(t.elapsed() < Duration::from_secs(20), "walk took {:?}", t.elapsed());
}

#[test]
fn memoized_helper_over_a_large_string_is_linear() {
    let n = 200_000;
    let src = "a".repeat(n);
    let t = Instant::now();
    let v = run(SRC, "walk_peek", vec![s(&src)]).unwrap();
    assert_eq!(v, Value::Int(n as i64));
    assert!(t.elapsed() < Duration::from_secs(20), "walk_peek took {:?}", t.elapsed());
}

#[test]
fn find_hops_are_linear() {
    let n = 100_000;
    let src = "a,".repeat(n);
    let t = Instant::now();
    let v = run(SRC, "hops", vec![s(&src), Value::Int(n as i64)]).unwrap();
    assert_eq!(v, Value::Int(n as i64));
    assert!(t.elapsed() < Duration::from_secs(20), "hops took {:?}", t.elapsed());
}
