//! End-to-end tests for #331, #332, and #334.

use lex_ast::canonicalize_program;
use lex_bytecode::vm::Vm;
use lex_bytecode::Value;
use lex_runtime::{check_program, DefaultHandler, Policy};
use lex_syntax::parse_source;

fn run_one(src: &str, entry: &str, args: Vec<Value>) -> Value {
    let prog = parse_source(src).unwrap();
    let stages = canonicalize_program(&prog);
    let bc = lex_bytecode::compile_program(&stages);
    let policy = Policy::pure();
    check_program(&bc, &policy).expect("type-checks");
    let handler = DefaultHandler::new(policy);
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    vm.call(entry, args).unwrap()
}


// ── #332: Str < Str at runtime ───────────────────────────────────────────────

#[test]
fn str_lt_works() {
    let src = r#"fn f(a :: Str, b :: Str) -> Bool { a < b }"#;
    assert_eq!(run_one(src, "f", vec![Value::Str("abc".into()), Value::Str("abd".into())]), Value::Bool(true));
    assert_eq!(run_one(src, "f", vec![Value::Str("abd".into()), Value::Str("abc".into())]), Value::Bool(false));
    assert_eq!(run_one(src, "f", vec![Value::Str("abc".into()), Value::Str("abc".into())]), Value::Bool(false));
}

#[test]
fn str_le_works() {
    let src = r#"fn f(a :: Str, b :: Str) -> Bool { a <= b }"#;
    assert_eq!(run_one(src, "f", vec![Value::Str("abc".into()), Value::Str("abc".into())]), Value::Bool(true));
    assert_eq!(run_one(src, "f", vec![Value::Str("abd".into()), Value::Str("abc".into())]), Value::Bool(false));
}

#[test]
fn str_gt_works() {
    let src = r#"fn f(a :: Str, b :: Str) -> Bool { a > b }"#;
    assert_eq!(run_one(src, "f", vec![Value::Str("b".into()), Value::Str("a".into())]), Value::Bool(true));
}

#[test]
fn str_ge_works() {
    let src = r#"fn f(a :: Str, b :: Str) -> Bool { a >= b }"#;
    assert_eq!(run_one(src, "f", vec![Value::Str("a".into()), Value::Str("a".into())]), Value::Bool(true));
}

// ── #334: list.cons ───────────────────────────────────────────────────────────

#[test]
fn list_cons_prepends() {
    let src = r#"
import "std.list" as list
fn f(x :: Int, xs :: List[Int]) -> List[Int] { list.cons(x, xs) }
"#;
    assert_eq!(
        run_one(src, "f", vec![Value::Int(1), Value::List(vec![Value::Int(2), Value::Int(3)].into())]),
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into())
    );
}

#[test]
fn list_cons_onto_empty() {
    let src = r#"
import "std.list" as list
fn f(x :: Int) -> List[Int] { list.cons(x, []) }
"#;
    assert_eq!(
        run_one(src, "f", vec![Value::Int(42)]),
        Value::List(vec![Value::Int(42)].into())
    );
}

#[test]
fn list_cons_reverse_build_pattern() {
    // idiomatic O(n) builder: cons in reverse, reverse at the end
    let src = r#"
import "std.list" as list
fn build(n :: Int) -> List[Int] { build_rev(n, []) }
fn build_rev(n :: Int, acc :: List[Int]) -> List[Int] {
  if n <= 0 { list.reverse(acc) } else { build_rev(n - 1, list.cons(n, acc)) }
}
"#;
    // cons(4,[]) → cons(3,[4]) → cons(2,[3,4]) → cons(1,[2,3,4]) → reverse → [4,3,2,1]
    assert_eq!(
        run_one(src, "build", vec![Value::Int(4)]),
        Value::List(vec![Value::Int(4), Value::Int(3), Value::Int(2), Value::Int(1)].into())
    );
}

// ── #331: datetime.before / after / compare + duration.seconds ───────────────

#[test]
fn datetime_before_after() {
    let src = r#"
import "std.datetime" as datetime
fn earlier(a :: Str, b :: Str) -> Bool {
  match datetime.parse_iso(a) {
    Ok(ai) => match datetime.parse_iso(b) {
      Ok(bi) => datetime.before(ai, bi),
      Err(_) => false,
    },
    Err(_) => false,
  }
}
fn later(a :: Str, b :: Str) -> Bool {
  match datetime.parse_iso(a) {
    Ok(ai) => match datetime.parse_iso(b) {
      Ok(bi) => datetime.after(ai, bi),
      Err(_) => false,
    },
    Err(_) => false,
  }
}
"#;
    let t1 = Value::Str("2024-01-01T00:00:00Z".into());
    let t2 = Value::Str("2024-06-01T00:00:00Z".into());
    assert_eq!(run_one(src, "earlier", vec![t1.clone(), t2.clone()]), Value::Bool(true));
    assert_eq!(run_one(src, "earlier", vec![t2.clone(), t1.clone()]), Value::Bool(false));
    assert_eq!(run_one(src, "later",   vec![t2.clone(), t1.clone()]), Value::Bool(true));
    assert_eq!(run_one(src, "later",   vec![t1.clone(), t2.clone()]), Value::Bool(false));
}

#[test]
fn datetime_compare() {
    let src = r#"
import "std.datetime" as datetime
fn cmp(a :: Str, b :: Str) -> Int {
  match datetime.parse_iso(a) {
    Ok(ai) => match datetime.parse_iso(b) {
      Ok(bi) => datetime.compare(ai, bi),
      Err(_) => 0,
    },
    Err(_) => 0,
  }
}
"#;
    let t1 = Value::Str("2024-01-01T00:00:00Z".into());
    let t2 = Value::Str("2024-06-01T00:00:00Z".into());
    assert_eq!(run_one(src, "cmp", vec![t1.clone(), t2.clone()]), Value::Int(-1));
    assert_eq!(run_one(src, "cmp", vec![t2.clone(), t1.clone()]), Value::Int(1));
    assert_eq!(run_one(src, "cmp", vec![t1.clone(), t1.clone()]), Value::Int(0));
}

#[test]
fn duration_seconds_extraction() {
    let src = r#"
import "std.datetime" as datetime
import "std.duration" as duration
fn elapsed_seconds(a :: Str, b :: Str) -> Int {
  match datetime.parse_iso(a) {
    Ok(ai) => match datetime.parse_iso(b) {
      Ok(bi) => duration.seconds(datetime.diff(bi, ai)),
      Err(_) => 0,
    },
    Err(_) => 0,
  }
}
"#;
    let t1 = Value::Str("2024-01-01T00:00:00Z".into());
    let t2 = Value::Str("2024-01-01T00:01:00Z".into()); // 60 seconds later
    assert_eq!(run_one(src, "elapsed_seconds", vec![t1, t2]), Value::Int(60));
}
