//! #464 tuple codegen — `Op::AllocStackTuple` end-to-end tests.
//!
//! Exercises the escape-analysis-driven lowering of non-escaping
//! `MakeTuple` to `AllocStackTuple`, the VM's shared stack-record
//! arena bookkeeping for tuples, `Op::GetElem` dispatch over both
//! `Value::Tuple` and `Value::StackTuple`, the per-frame budget
//! fallback, and the body-hash canonicality contract (#222).
//!
//! Tuple elements are read via pattern destructuring (`match`), the
//! only surface construct that emits `GetElem` — and a destructured-
//! then-dropped tuple is exactly the non-escaping shape the analysis
//! proves frame-local.

use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Op, Value, Vm};
use lex_syntax::parse_source;

fn compile(src: &str) -> lex_bytecode::Program {
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    compile_program(&stages)
}

fn fn_code<'a>(prog: &'a lex_bytecode::Program, name: &str) -> &'a [Op] {
    let idx = prog.function_names[name];
    &prog.functions[idx as usize].code
}

fn count<F: Fn(&Op) -> bool>(code: &[Op], pred: F) -> usize {
    code.iter().filter(|op| pred(op)).count()
}

/// A tuple built, destructured, and dropped never escapes the frame.
/// The lowering pass must replace `MakeTuple` with `AllocStackTuple`,
/// and the program must run with identical semantics.
#[test]
fn non_escaping_tuple_lowers_to_alloc_stack_tuple() {
    let src = r#"
        fn add_pair() -> Int {
          match (3, 4) { (a, b) => a + b }
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "add_pair");
    assert_eq!(count(code, |op| matches!(op, Op::MakeTuple(_))), 0,
        "MakeTuple should have been lowered: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackTuple { .. })), 1,
        "expected exactly one AllocStackTuple: {code:?}");

    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("add_pair", vec![]).unwrap(), Value::Int(7));
    // Prove the stack path actually ran at runtime, not just that the
    // op was emitted at compile time.
    assert!(vm.stack_record_allocs > 0, "stack-tuple path should have fired");
}

/// A tuple that is returned escapes. The lowering pass must leave it
/// as `MakeTuple` so it lives on the heap as `Value::Tuple`.
#[test]
fn escaping_tuple_stays_on_heap() {
    let src = r#"
        fn build() -> Tuple[Int, Int] { (1, 2) }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "build");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackTuple { .. })), 0,
        "escaping tuple must not be stack-allocated: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::MakeTuple(_))), 1);

    let mut vm = Vm::new(&p);
    match vm.call("build", vec![]).unwrap() {
        Value::Tuple(items) => assert_eq!(items, vec![Value::Int(1), Value::Int(2)]),
        other => panic!("expected Tuple, got {other:?}"),
    }
}

/// A tuple passed to a call escapes (the callee's body is opaque to
/// the intra-procedural analysis), so it stays on the heap.
#[test]
fn tuple_passed_to_call_stays_on_heap() {
    let src = r#"
        fn use_pair(p :: Tuple[Int, Int]) -> Int { match p { (a, b) => a + b } }
        fn caller() -> Int {
          let t := (5, 6)
          use_pair(t)
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "caller");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackTuple { .. })), 0,
        "tuple passed to a call must stay on heap: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::MakeTuple(_))), 1);

    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("caller", vec![]).unwrap(), Value::Int(11));
}

/// Element reads over a stack-allocated tuple go through the same
/// `Op::GetElem` the heap path uses — dispatch is polymorphic over
/// both `Value::Tuple` and `Value::StackTuple`. A 3-tuple exercises
/// more than one positional index.
#[test]
fn stack_tuple_elem_reads_are_polymorphic() {
    let src = r#"
        fn sum_triple() -> Int {
          match (10, 20, 30) { (a, b, c) => a + b + c }
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "sum_triple");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackTuple { .. })), 1);

    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("sum_triple", vec![]).unwrap(), Value::Int(60));
}

/// Body-hash invariance under the lowering (#222). The canonical
/// encoding decodes `AllocStackTuple` as `MakeTuple`, so two
/// source-equivalent functions hash identically whether or not the
/// escape pass fired.
#[test]
fn body_hash_invariant_under_tuple_lowering() {
    let src_a = r#"
        fn a() -> Int { match (5, 6) { (x, y) => x + y } }
    "#;
    let src_b = r#"
        fn b() -> Int { match (5, 6) { (x, y) => x + y } }
    "#;
    let pa = compile(src_a);
    let pb = compile(src_b);
    let fa = &pa.functions[pa.function_names["a"] as usize];
    let fb = &pb.functions[pb.function_names["b"] as usize];
    assert_eq!(fa.body_hash, fb.body_hash);
    assert!(fa.code.iter().any(|op| matches!(op, Op::AllocStackTuple { .. })),
        "expected the tuple to lower: {:?}", fa.code);
}

/// The verifier walks `AllocStackTuple` with the same `-(arity)+1`
/// stack delta as `MakeTuple`, so lowered code still verifies.
#[test]
fn verifier_accepts_alloc_stack_tuple() {
    let src = r#"
        fn ok() -> Int { match (1, 2, 3) { (a, b, c) => a + b + c } }
    "#;
    let p = compile(src);
    let errs = lex_bytecode::verify_program(&p.functions);
    assert!(errs.is_empty(), "verifier should accept lowered code: {errs:?}");
}

/// Budget exhaustion: a frame that allocates more stack-tuple slots
/// than `STACK_RECORD_BUDGET_SLOTS` allows silently falls back to the
/// heap `Value::Tuple` path for the overflow. All sites still lower at
/// compile time; the result is identical regardless of which path each
/// allocation took, and both the stack and fallback counters fire.
#[test]
fn budget_exhaustion_falls_back_for_tuples() {
    let mut src = String::from("fn many() -> Int {\n");
    for i in 0..70 {
        src.push_str(&format!("  let s{i} := match ({i}, 0) {{ (a, b) => a + b }}\n"));
    }
    let parts: Vec<String> = (0..70).map(|i| format!("s{i}")).collect();
    src.push_str("  ");
    src.push_str(&parts.join(" + "));
    src.push_str("\n}\n");

    let p = compile(&src);
    let code = fn_code(&p, "many");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackTuple { .. })), 70,
        "all 70 tuple sites should compile to AllocStackTuple");
    assert_eq!(count(code, |op| matches!(op, Op::MakeTuple(_))), 0,
        "no MakeTuple left after lowering");

    let mut vm = Vm::new(&p);
    let expected: i64 = (0..70).sum();
    assert_eq!(vm.call("many", vec![]).unwrap(), Value::Int(expected));
    // 70 tuples × 2 slots = 140 > 64-slot budget: the stack path runs
    // until the budget is spent, then the heap fallback takes over.
    assert!(vm.stack_record_allocs > 0, "stack path should have fired");
    assert!(vm.stack_record_heap_fallbacks > 0, "heap fallback should have fired");
}
