//! #464 — native `list.map` / `list.filter` / `list.fold` opcodes.
//!
//! These used to compile to inlined bytecode loops that re-`LoadLocal`'d
//! (cloned) the whole input and accumulator lists every iteration —
//! O(n²). They now lower to single native VM opcodes (`Op::ListMap` /
//! `ListFilter` / `ListFold`), mirroring `SortByKey` / `ParallelMap`.
//! These tests pin the observable semantics (the runtime-level tests
//! in lex-runtime exercise the same ops end-to-end in CI).

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Op, Program, Value, Vm};
use lex_syntax::parse_source;

fn compile(src: &str) -> Program {
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    lex_types::check_program(&stages).expect("typecheck");
    compile_program(&stages)
}

fn run(src: &str, func: &str) -> Value {
    let p = compile(src);
    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    vm.call(func, vec![]).unwrap()
}

fn ints(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items.iter().map(|x| match x {
            Value::Int(n) => *n,
            other => panic!("expected Int element, got {other:?}"),
        }).collect(),
        other => panic!("expected List, got {other:?}"),
    }
}

fn op_count<F: Fn(&Op) -> bool>(p: &Program, name: &str, pred: F) -> usize {
    let idx = p.function_names[name];
    p.functions[idx as usize].code.iter().filter(|o| pred(o)).count()
}

#[test]
fn map_doubles_and_preserves_order() {
    let src = r#"
        import "std.list" as list
        fn m() -> List[Int] {
          list.map([1, 2, 3, 4], fn(x :: Int) -> Int { x * 2 })
        }
    "#;
    let p = compile(src);
    // Lowered to a single native op, with no leftover inlined-loop ops.
    assert_eq!(op_count(&p, "m", |o| matches!(o, Op::ListMap { .. })), 1);
    assert_eq!(op_count(&p, "m", |o| matches!(o, Op::ListAppend)), 0,
        "no inlined ListAppend loop should remain");
    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    assert_eq!(ints(&vm.call("m", vec![]).unwrap()), vec![2, 4, 6, 8]);
}

#[test]
fn map_over_empty_list() {
    let src = r#"
        import "std.list" as list
        fn m(xs :: List[Int]) -> List[Int] {
          list.map(xs, fn(x :: Int) -> Int { x + 1 })
        }
    "#;
    let p = compile(src);
    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    let r = vm.call("m", vec![Value::List(Default::default())]).unwrap();
    assert_eq!(ints(&r), Vec::<i64>::new());
}

#[test]
fn filter_keeps_matching_in_order() {
    let src = r#"
        import "std.list" as list
        fn f() -> List[Int] {
          list.filter([1, 2, 3, 4, 5], fn(x :: Int) -> Bool { x > 2 })
        }
    "#;
    let p = compile(src);
    assert_eq!(op_count(&p, "f", |o| matches!(o, Op::ListFilter { .. })), 1);
    assert_eq!(ints(&run(src, "f")), vec![3, 4, 5]);
}

#[test]
fn filter_can_drop_everything() {
    let src = r#"
        import "std.list" as list
        fn f() -> List[Int] {
          list.filter([1, 2, 3], fn(x :: Int) -> Bool { x > 100 })
        }
    "#;
    assert_eq!(ints(&run(src, "f")), Vec::<i64>::new());
}

#[test]
fn fold_sums() {
    let src = r#"
        import "std.list" as list
        fn s() -> Int {
          list.fold([1, 2, 3, 4], 0, fn(acc :: Int, x :: Int) -> Int { acc + x })
        }
    "#;
    let p = compile(src);
    assert_eq!(op_count(&p, "s", |o| matches!(o, Op::ListFold { .. })), 1);
    assert_eq!(run(src, "s"), Value::Int(10));
}

#[test]
fn fold_is_left_associative() {
    // ((((0*10+1)*10+2)*10+3) = 123 — a non-commutative combiner pins
    // the left-to-right order.
    let src = r#"
        import "std.list" as list
        fn s() -> Int {
          list.fold([1, 2, 3], 0, fn(acc :: Int, x :: Int) -> Int { acc * 10 + x })
        }
    "#;
    assert_eq!(run(src, "s"), Value::Int(123));
}

#[test]
fn fold_over_empty_returns_init() {
    let src = r#"
        import "std.list" as list
        fn s(xs :: List[Int]) -> Int {
          list.fold(xs, 42, fn(acc :: Int, x :: Int) -> Int { acc + x })
        }
    "#;
    let p = compile(src);
    let mut vm = Vm::new(&p);
    vm.set_step_limit(u64::MAX);
    let r = vm.call("s", vec![Value::List(Default::default())]).unwrap();
    assert_eq!(r, Value::Int(42));
}

#[test]
fn nested_map_then_fold() {
    let src = r#"
        import "std.list" as list
        fn pipeline() -> Int {
          let doubled := list.map([1, 2, 3, 4], fn(x :: Int) -> Int { x * 2 })
          let big := list.filter(doubled, fn(x :: Int) -> Bool { x > 3 })
          list.fold(big, 0, fn(acc :: Int, x :: Int) -> Int { acc + x })
        }
    "#;
    // doubled = [2,4,6,8]; big = [4,6,8]; sum = 18.
    assert_eq!(run(src, "pipeline"), Value::Int(18));
}
