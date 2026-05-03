//! std.map and std.set — persistent collections with `Str` or `Int`
//! keys. Tests both that the type signatures unify and that the
//! runtime returns the right value.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, vm::Vm, MapKey, Value};
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

#[test]
fn map_set_get_roundtrip() {
    let src = r#"
import "std.map" as map
fn build() -> Map[Str, Int] {
  let m1 := map.set(map.new(), "a", 1)
  let m2 := map.set(m1, "b", 2)
  m2
}
fn lookup(m :: Map[Str, Int], k :: Str) -> Int {
  match map.get(m, k) {
    Some(n) => n,
    None    => 0,
  }
}
fn demo() -> Int {
  let m := build()
  lookup(m, "a") + lookup(m, "b") + lookup(m, "missing")
}
"#;
    assert_eq!(run(src, "demo", vec![]), Value::Int(3));
}

#[test]
fn map_size_grows_with_inserts() {
    let src = r#"
import "std.map" as map
fn count() -> Int {
  let m := map.set(map.set(map.set(map.new(), "x", 1), "y", 2), "z", 3)
  map.size(m)
}
"#;
    assert_eq!(run(src, "count", vec![]), Value::Int(3));
}

#[test]
fn map_set_overwrites_existing_value() {
    let src = r#"
import "std.map" as map
fn overwrite() -> Int {
  let m1 := map.set(map.new(), "k", 10)
  let m2 := map.set(m1, "k", 99)
  match map.get(m2, "k") {
    Some(n) => n,
    None    => -1,
  }
}
"#;
    assert_eq!(run(src, "overwrite", vec![]), Value::Int(99));
}

#[test]
fn map_delete_drops_key() {
    let src = r#"
import "std.map" as map
fn after_delete() -> Bool {
  let m1 := map.set(map.new(), "k", 1)
  let m2 := map.delete(m1, "k")
  map.has(m2, "k")
}
"#;
    assert_eq!(run(src, "after_delete", vec![]), Value::Bool(false));
}

#[test]
fn map_int_keys_work() {
    let src = r#"
import "std.map" as map
fn pick(m :: Map[Int, Str], k :: Int) -> Str {
  match map.get(m, k) {
    Some(s) => s,
    None    => "missing",
  }
}
fn build() -> Map[Int, Str] {
  map.set(map.set(map.new(), 1, "one"), 2, "two")
}
fn demo() -> Str { pick(build(), 2) }
"#;
    assert_eq!(run(src, "demo", vec![]), Value::Str("two".into()));
}

#[test]
fn map_from_list_round_trips_through_entries() {
    let src = r#"
import "std.map" as map
import "std.list" as list
fn count() -> Int {
  let m := map.from_list([("a", 1), ("b", 2), ("c", 3)])
  list.fold(map.values(m), 0, fn (acc :: Int, n :: Int) -> Int { acc + n })
}
"#;
    assert_eq!(run(src, "count", vec![]), Value::Int(6));
}

#[test]
fn set_dedupes_a_list() {
    let src = r#"
import "std.set" as set
import "std.list" as list
fn unique_sum() -> Int {
  let s := set.from_list([3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5])
  list.fold(set.to_list(s), 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
}
"#;
    // {1, 2, 3, 4, 5, 6, 9} = 30.
    assert_eq!(run(src, "unique_sum", vec![]), Value::Int(30));
}

#[test]
fn set_union_intersect() {
    let src = r#"
import "std.set" as set
fn check() -> Int {
  let a := set.from_list([1, 2, 3])
  let b := set.from_list([2, 3, 4])
  let u := set.union(a, b)         # {1,2,3,4} -> 4
  let i := set.intersect(a, b)     # {2,3}     -> 2
  set.size(u) + set.size(i)
}
"#;
    assert_eq!(run(src, "check", vec![]), Value::Int(6));
}

#[test]
fn map_of_non_str_or_int_key_is_runtime_error() {
    // Keys are typed polymorphically (`Var(0)`) but only `Str` /
    // `Int` are actually permitted at runtime — anything else
    // surfaces as `expected Map, got ...` style errors when we
    // try to convert the key.
    let src = r#"
import "std.map" as map
fn bad() -> Bool { map.has(map.set(map.new(), [1, 2], 1), [3, 4]) }
"#;
    let prog = parse_source(src).expect("parse");
    let stages = canonicalize_program(&prog);
    // This may or may not type-check (the type variable is opaque
    // to the runtime); we care that it doesn't reach `Some(true)`.
    let bc = compile_program(&stages);
    let handler = DefaultHandler::new(Policy::permissive());
    let mut vm = Vm::with_handler(&bc, Box::new(handler));
    let r = vm.call("bad", vec![]);
    assert!(r.is_err(), "expected runtime error for non-primitive key");
}

#[test]
fn map_value_uses_btreemap_directly() {
    // The Value::Map variant exposes the BTreeMap API so Rust
    // code (e.g. handlers) can read it without going through
    // the dispatcher. Sanity check the variant shape.
    let src = r#"
import "std.map" as map
fn build() -> Map[Str, Int] { map.set(map.set(map.new(), "x", 7), "y", 11) }
"#;
    let v = run(src, "build", vec![]);
    let m = match v { Value::Map(m) => m, other => panic!("not a Map: {other:?}") };
    assert_eq!(m.len(), 2);
    assert_eq!(m.get(&MapKey::Str("x".into())), Some(&Value::Int(7)));
    assert_eq!(m.get(&MapKey::Str("y".into())), Some(&Value::Int(11)));
}

#[test]
fn map_fold_sums_values() {
    // Combiner uses only `v`, ignores `k`. Smoke test the wiring.
    let src = r#"
import "std.map" as map
fn sum_values() -> Int {
  let m := map.set(map.set(map.set(map.new(), "a", 1), "b", 2), "c", 3)
  map.fold(m, 0, fn (acc :: Int, k :: Str, v :: Int) -> Int { acc + v })
}
"#;
    assert_eq!(run(src, "sum_values", vec![]), Value::Int(6));
}

#[test]
fn map_fold_on_empty_returns_init() {
    let src = r#"
import "std.map" as map
fn fold_empty() -> Int {
  map.fold(map.new(), 42, fn (acc :: Int, k :: Str, v :: Int) -> Int { acc + v })
}
"#;
    assert_eq!(run(src, "fold_empty", vec![]), Value::Int(42));
}

#[test]
fn map_fold_passes_both_key_and_value_to_combiner() {
    // Combiner uses both: `acc + parse(k) * v`. With keys "1","2","3"
    // and values 10,20,30 → 1*10 + 2*20 + 3*30 = 140.
    let src = r#"
import "std.map" as map
import "std.str" as str
fn weighted_sum() -> Int {
  let m := map.set(map.set(map.set(map.new(), "1", 10), "2", 20), "3", 30)
  map.fold(m, 0, fn (acc :: Int, k :: Str, v :: Int) -> Int {
    let kn := match str.to_int(k) { Some(n) => n, None => 0 }
    acc + kn * v
  })
}
"#;
    assert_eq!(run(src, "weighted_sum", vec![]), Value::Int(140));
}

#[test]
fn map_fold_iterates_in_btreemap_key_order() {
    // BTreeMap iteration is sorted by key. Folding into a list shows
    // the order: insertion was "z","a","m" but iteration is "a","m","z".
    let src = r#"
import "std.map" as map
import "std.list" as list
fn keys_in_iter_order() -> List[Str] {
  let m := map.set(map.set(map.set(map.new(), "z", 1), "a", 2), "m", 3)
  map.fold(m, [], fn (acc :: List[Str], k :: Str, v :: Int) -> List[Str] {
    list.concat(acc, [k])
  })
}
"#;
    assert_eq!(
        run(src, "keys_in_iter_order", vec![]),
        Value::List(vec![
            Value::Str("a".into()),
            Value::Str("m".into()),
            Value::Str("z".into()),
        ])
    );
}

#[test]
fn map_fold_works_with_int_keys() {
    let src = r#"
import "std.map" as map
fn sum_int_keys() -> Int {
  let m := map.set(map.set(map.set(map.new(), 10, 100), 20, 200), 30, 300)
  map.fold(m, 0, fn (acc :: Int, k :: Int, v :: Int) -> Int { acc + k + v })
}
"#;
    // 10+100 + 20+200 + 30+300 = 660
    assert_eq!(run(src, "sum_int_keys", vec![]), Value::Int(660));
}

#[test]
fn map_fold_combiner_can_capture_outer_locals() {
    let src = r#"
import "std.map" as map
fn weighted() -> Int {
  let weight := 100
  let m := map.set(map.set(map.new(), "a", 1), "b", 2)
  map.fold(m, 0, fn (acc :: Int, k :: Str, v :: Int) -> Int { acc + v * weight })
}
"#;
    assert_eq!(run(src, "weighted", vec![]), Value::Int(300));
}
