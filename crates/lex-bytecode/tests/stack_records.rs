//! #464 step 2 — `Op::AllocStackRecord` end-to-end tests.
//!
//! Exercises the compiler's escape-analysis-driven lowering pass,
//! the VM's stack-record arena bookkeeping, the polymorphic
//! `Op::GetField` IC dispatch over both `Value::Record` and
//! `Value::StackRecord`, the per-frame budget fallback, and the
//! body-hash canonicality contract.

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

/// A record that is allocated and then field-read and dropped never
/// escapes the frame. The lowering pass must replace `MakeRecord`
/// with `AllocStackRecord`, and the program must still run with the
/// same semantics.
#[test]
fn non_escaping_record_lowers_to_alloc_stack_record() {
    let src = r#"
        fn drop_and_read() -> Int {
          let r := { x: 7, y: 9 }
          r.x
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "drop_and_read");
    assert_eq!(count(code, |op| matches!(op, Op::MakeRecord { .. })), 0,
        "MakeRecord should have been lowered: {code:?}");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 1,
        "expected exactly one AllocStackRecord: {code:?}");

    let mut vm = Vm::new(&p);
    let r = vm.call("drop_and_read", vec![]).unwrap();
    assert_eq!(r, Value::Int(7));
}

/// A record that is returned escapes the frame, so the stack pass
/// must leave it alone. Under #463 slice 2b-i the arena pass then
/// picks it up — the value crosses the frame but stays inside the
/// request scope — so the site lowers to `AllocArenaRecord` (the
/// middle tier), not `MakeRecord` (heap). Same observable semantics.
#[test]
fn escaping_record_stays_on_heap() {
    let src = r#"
        fn build() -> { x :: Int, y :: Int } {
          { x: 1, y: 2 }
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "build");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 0,
        "escaping record must not be stack-allocated: {code:?}");
    // Slice-2b three-tier: stays off stack, lands on arena. Pre-slice-2b
    // this asserted `MakeRecord == 1`; now the arena pass takes it.
    assert_eq!(count(code, |op| matches!(op, Op::MakeRecord { .. })), 0);
    assert_eq!(count(code, |op| matches!(op, Op::AllocArenaRecord { .. })), 1,
        "frame-escaping but request-local record should lower to AllocArenaRecord: {code:?}");

    let mut vm = Vm::new(&p);
    let r = vm.call("build", vec![]).unwrap();
    match r {
        Value::Record { fields, .. } => {
            assert_eq!(fields.get("x"), Some(&Value::Int(1)));
            assert_eq!(fields.get("y"), Some(&Value::Int(2)));
        }
        other => panic!("expected Record, got {other:?}"),
    }
}

/// A stack-allocated record's field read goes through the same
/// `Op::GetField` opcode the heap path uses — the dispatch is
/// polymorphic over both `Value::Record` and `Value::StackRecord`.
/// Multiple field reads on the same record exercise the inline cache.
#[test]
fn stack_record_field_reads_use_polymorphic_ic() {
    let src = r#"
        fn sum_fields() -> Int {
          let r := { a: 10, b: 20, c: 30 }
          r.a + r.b + r.c
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "sum_fields");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 1);

    let mut vm = Vm::new(&p);
    let r = vm.call("sum_fields", vec![]).unwrap();
    assert_eq!(r, Value::Int(60));
}

/// Two non-escaping `MakeRecord` sites in the same function lower
/// independently. The arena is shared, but the per-record
/// `slab_start` differs.
#[test]
fn two_stack_records_in_one_frame() {
    let src = r#"
        fn two_records() -> Int {
          let r1 := { x: 100 }
          let r2 := { y: 25 }
          r1.x + r2.y
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "two_records");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 2,
        "both records should lower: {code:?}");

    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("two_records", vec![]).unwrap(), Value::Int(125));
}

/// A record allocated inside a helper call must NOT be lowered if
/// the helper returns it — that's an escape. Verifies the
/// inter-procedural conservatism the design doc commits to.
#[test]
fn record_returned_from_helper_is_not_lowered() {
    let src = r#"
        fn make_point() -> { x :: Int, y :: Int } { { x: 3, y: 4 } }
        fn caller() -> Int {
          let p := make_point()
          p.x + p.y
        }
    "#;
    let p = compile(src);
    let make_point_code = fn_code(&p, "make_point");
    assert_eq!(count(make_point_code, |op| matches!(op, Op::AllocStackRecord { .. })), 0,
        "helper-returned record must stay off stack: {make_point_code:?}");
    // Slice-2b three-tier: same shape as `escaping_record_stays_on_heap`
    // — the returned record stays off stack and lands on arena.
    assert_eq!(count(make_point_code, |op| matches!(op, Op::MakeRecord { .. })), 0);
    assert_eq!(count(make_point_code, |op| matches!(op, Op::AllocArenaRecord { .. })), 1);

    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("caller", vec![]).unwrap(), Value::Int(7));
}

/// Body-hash invariance under the lowering pass (#222). The
/// canonical encoding decodes `AllocStackRecord` as the historical
/// `MakeRecord` form, so two source-equivalent functions hash
/// identically regardless of whether the analysis fired.
#[test]
fn body_hash_invariant_under_lowering() {
    let src_a = r#"
        fn a() -> Int {
          let r := { x: 5 }
          r.x
        }
    "#;
    let src_b = r#"
        fn b() -> Int {
          let r := { x: 5 }
          r.x
        }
    "#;
    let pa = compile(src_a);
    let pb = compile(src_b);
    let fa = &pa.functions[pa.function_names["a"] as usize];
    let fb = &pb.functions[pb.function_names["b"] as usize];
    assert_eq!(fa.body_hash, fb.body_hash);
    assert!(fa.code.iter().any(|op| matches!(op, Op::AllocStackRecord { .. })));
}

/// Stack delta is unchanged by the lowering. The verifier walks
/// `AllocStackRecord` with the same -(n)+1 delta as `MakeRecord`.
#[test]
fn verifier_accepts_alloc_stack_record() {
    let src = r#"
        fn ok() -> Int {
          let r := { p: 1, q: 2, s: 3 }
          r.q
        }
    "#;
    let p = compile(src);
    let errs = lex_bytecode::verify_program(&p.functions);
    assert!(errs.is_empty(), "verifier should accept lowered code: {errs:?}");
}

/// Budget exhaustion: when a frame allocates more stack records
/// than `STACK_RECORD_BUDGET_SLOTS` allows, further allocations
/// fall back to the heap path silently. Observable result is
/// identical.
#[test]
fn budget_exhaustion_falls_back_to_heap_without_changing_result() {
    let mut src = String::from("fn many() -> Int {\n");
    for i in 0..70 {
        src.push_str(&format!("  let r{i} := {{ v: {i} }}\n"));
    }
    src.push_str("  ");
    let parts: Vec<String> = (0..70).map(|i| format!("r{i}.v")).collect();
    src.push_str(&parts.join(" + "));
    src.push_str("\n}\n");

    let p = compile(&src);
    let code = fn_code(&p, "many");
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 70,
        "all 70 sites should compile to AllocStackRecord");
    assert_eq!(count(code, |op| matches!(op, Op::MakeRecord { .. })), 0,
        "no MakeRecord left after lowering");

    let mut vm = Vm::new(&p);
    let expected: i64 = (0..70).sum();
    assert_eq!(vm.call("many", vec![]).unwrap(), Value::Int(expected));
}

/// Mixed escape pattern: in the same function, one site escapes
/// and another doesn't. Lowering must be per-site, not all-or-nothing.
#[test]
fn per_site_lowering_in_mixed_function() {
    let src = r#"
        fn mix() -> { z :: Int } {
          let temp := { a: 1, b: 2 }
          let _ := temp.a
          { z: 99 }
        }
    "#;
    let p = compile(src);
    let code = fn_code(&p, "mix");
    // Three-tier: dropped record → stack (cheapest tier);
    // returned record → arena (middle tier, slice 2b-i);
    // no remaining heap MakeRecord in this function.
    assert_eq!(count(code, |op| matches!(op, Op::AllocStackRecord { .. })), 1,
        "the dropped {{a,b}} record should lower to stack");
    assert_eq!(count(code, |op| matches!(op, Op::AllocArenaRecord { .. })), 1,
        "the returned {{z}} record should lower to arena");
    assert_eq!(count(code, |op| matches!(op, Op::MakeRecord { .. })), 0,
        "no record in this function should remain on the heap tier");

    let mut vm = Vm::new(&p);
    let r = vm.call("mix", vec![]).unwrap();
    match r {
        Value::Record { fields, .. } => {
            assert_eq!(fields.get("z"), Some(&Value::Int(99)));
        }
        other => panic!("expected Record, got {other:?}"),
    }
}
