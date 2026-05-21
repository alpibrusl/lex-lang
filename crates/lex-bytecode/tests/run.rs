//! M4 acceptance: pure §3.13 examples produce expected outputs.

use indexmap::IndexMap;
use lex_ast::canonicalize_program;
use lex_bytecode::{compile_program, Value, Vm, VmError, MAX_CALL_DEPTH};
use lex_syntax::parse_source;

fn compile(src: &str) -> lex_bytecode::Program {
    let p = parse_source(src).unwrap();
    let stages = canonicalize_program(&p);
    compile_program(&stages)
}

#[test]
fn unbounded_recursion_yields_call_stack_overflow_not_segfault() {
    // Non-tail recursion (the `+ 1` forces the call to return before
    // we can use its result), so each call pushes a fresh frame.
    // Pre-fix the VM would push frames until the host's native stack
    // exploded; post-fix we get a clean `CallStackOverflow` once we
    // hit `MAX_CALL_DEPTH`.
    //
    // Run on a thread with a small stack so a regression (a recursion
    // path that bypasses `push_frame`) shows up as a SIGSEGV rather
    // than passing because the host stack happens to be 8 MiB.
    let src = "fn deep() -> Int { 1 + deep() }\n";
    let p = compile(src);
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut vm = Vm::new(&p);
            vm.call("deep", vec![])
        })
        .expect("spawn worker thread");
    let r = handle.join().expect("worker panicked").expect_err("expected overflow");
    match r {
        VmError::CallStackOverflow(n) => assert_eq!(n, MAX_CALL_DEPTH),
        other => panic!("expected CallStackOverflow, got {other:?}"),
    }
}

#[test]
fn modest_recursion_under_cap_still_runs() {
    // factorial(20) recurses 20 frames — well under MAX_CALL_DEPTH.
    // Sanity check that the gate doesn't reject legitimate code.
    let src = "fn factorial(n :: Int) -> Int { match n { 0 => 1, _ => n * factorial(n - 1) } }\n";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("factorial", vec![Value::Int(20)]).unwrap();
    assert_eq!(r, Value::Int(2_432_902_008_176_640_000));
}

#[test]
fn example_a_factorial() {
    let src = include_str!("../../../examples/a_factorial.lex");
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("factorial", vec![Value::Int(5)]).unwrap();
    assert_eq!(r, Value::Int(120));
    let r = vm.call("factorial", vec![Value::Int(0)]).unwrap();
    assert_eq!(r, Value::Int(1));
    let r = vm.call("factorial", vec![Value::Int(10)]).unwrap();
    assert_eq!(r, Value::Int(3628800));
}

#[test]
fn example_d_shape() {
    let src = include_str!("../../../examples/d_shape.lex");
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let circle = Value::Variant {
        name: "Circle".into(),
        args: vec![Value::record_dynamic({
            let mut m = IndexMap::new();
            m.insert("radius".into(), Value::Float(1.0));
            m
        })],
    };
    let r = vm.call("area", vec![circle]).unwrap();
    let v = match r { Value::Float(f) => f, other => panic!("expected float, got {other:?}") };
    // Source uses 3.14159 directly (the spec's example, not std::f64::consts::PI).
    #[allow(clippy::approx_constant)]
    let expected_area = 3.14159_f64;
    assert!((v - expected_area).abs() < 1e-6, "got {v}");

    let rect = Value::Variant {
        name: "Rect".into(),
        args: vec![Value::record_dynamic({
            let mut m = IndexMap::new();
            m.insert("width".into(), Value::Float(2.0));
            m.insert("height".into(), Value::Float(3.0));
            m
        })],
    };
    let r = vm.call("area", vec![rect]).unwrap();
    assert_eq!(r, Value::Float(6.0));
}

#[test]
fn bytecode_is_reproducible() {
    let src = include_str!("../../../examples/a_factorial.lex");
    let p1 = compile(src);
    let p2 = compile(src);
    assert_eq!(p1, p2);
}

#[test]
fn match_with_literal_int() {
    let src = "fn id_or_zero(n :: Int) -> Int {\n  match n {\n    0 => 0,\n    _ => n,\n  }\n}\n";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("id_or_zero", vec![Value::Int(0)]).unwrap(), Value::Int(0));
    assert_eq!(vm.call("id_or_zero", vec![Value::Int(7)]).unwrap(), Value::Int(7));
}

#[test]
fn slice3_fuses_two_local_add_and_runs_correctly() {
    // #461 slice 3: `LoadLocal + LoadLocal + IntAdd` over two
    // Int-typed locals must (a) be rewritten to
    // `Op::LoadLocalAddLocal` by the peephole pass, (b) leave the
    // trailing two slots as untouched primitive tombstones (so the
    // body hash stays bit-identical to the unfused form), and
    // (c) produce the same numeric result as the unfused triple.
    use lex_bytecode::op::Op;
    let src = "fn add_them(a :: Int, b :: Int) -> Int { a + b }\n";
    let p = compile(src);
    let f = &p.functions[0];

    // The body should be exactly:
    //   [LoadLocalAddLocal{a,b}, LoadLocal(b) tombstone, IntAdd tombstone, Return]
    // — slice 3 rewrites slot 0; slots 1+2 stay live for body-hash
    // stability and are skipped by the dispatch loop via pc+=3.
    assert!(
        matches!(f.code[0], Op::LoadLocalAddLocal { lhs_idx: 0, rhs_idx: 1 }),
        "slice 3 did not fire; got {:?}", f.code[0],
    );
    assert!(matches!(f.code[1], Op::LoadLocal(1)));
    assert!(matches!(f.code[2], Op::IntAdd));
    assert!(matches!(f.code[3], Op::Return));

    let mut vm = Vm::new(&p);
    let r = vm.call("add_them", vec![Value::Int(40), Value::Int(2)]).unwrap();
    assert_eq!(r, Value::Int(42));
}

#[test]
fn slice4_fuses_two_local_sub_and_runs_correctly() {
    // #461 slice 4: same shape as slice 3 but for `IntSub`. Must
    // rewrite to `Op::LoadLocalSubLocal`, leave tombstones in place,
    // and compute lhs - rhs.
    use lex_bytecode::op::Op;
    let src = "fn sub_them(a :: Int, b :: Int) -> Int { a - b }\n";
    let p = compile(src);
    let f = &p.functions[0];
    assert!(
        matches!(f.code[0], Op::LoadLocalSubLocal { lhs_idx: 0, rhs_idx: 1 }),
        "slice 4 (sub) did not fire; got {:?}", f.code[0],
    );
    assert!(matches!(f.code[1], Op::LoadLocal(1)));
    assert!(matches!(f.code[2], Op::IntSub));
    assert!(matches!(f.code[3], Op::Return));

    let mut vm = Vm::new(&p);
    let r = vm.call("sub_them", vec![Value::Int(42), Value::Int(7)]).unwrap();
    assert_eq!(r, Value::Int(35));
}

#[test]
fn slice4_fuses_two_local_mul_and_runs_correctly() {
    use lex_bytecode::op::Op;
    let src = "fn mul_them(a :: Int, b :: Int) -> Int { a * b }\n";
    let p = compile(src);
    let f = &p.functions[0];
    assert!(
        matches!(f.code[0], Op::LoadLocalMulLocal { lhs_idx: 0, rhs_idx: 1 }),
        "slice 4 (mul) did not fire; got {:?}", f.code[0],
    );
    assert!(matches!(f.code[1], Op::LoadLocal(1)));
    assert!(matches!(f.code[2], Op::IntMul));
    assert!(matches!(f.code[3], Op::Return));

    let mut vm = Vm::new(&p);
    let r = vm.call("mul_them", vec![Value::Int(6), Value::Int(7)]).unwrap();
    assert_eq!(r, Value::Int(42));
}

#[test]
fn slice4_does_not_fire_across_a_jump_target() {
    // Mirror of `slice3_does_not_fire_across_a_jump_target` for sub/mul.
    // A `match`-arm body straddles a JumpIfNot target, so if the
    // jump-safety check ever regressed the call would panic or return
    // junk.
    let src = "
fn pick(flag :: Int, a :: Int, b :: Int) -> Int {
  match flag {
    0 => a - b,
    1 => a * b,
    _ => a,
  }
}";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("pick", vec![Value::Int(0), Value::Int(10), Value::Int(3)]).unwrap(), Value::Int(7));
    assert_eq!(vm.call("pick", vec![Value::Int(1), Value::Int(10), Value::Int(3)]).unwrap(), Value::Int(30));
    assert_eq!(vm.call("pick", vec![Value::Int(9), Value::Int(10), Value::Int(3)]).unwrap(), Value::Int(10));
}

#[test]
fn slice5_fuses_pattern_match_arm_test() {
    // #461 slice 5: every integer-literal pattern arm compiles to
    // `LoadLocal(scrut) + PushConst(lit) + IntEq + JumpIfNot(next_arm)`
    // after the compile_pattern_test typed-lowering (NumEq→IntEq for
    // Int literal patterns). Slice 5 fuses that window into
    // `Op::LoadLocalEqIntConstJumpIfNot`. Verify both that the fusion
    // fires and that the runtime semantics — `n == 0` arm vs the
    // recursive `_` arm — produce the same result as the unfused
    // form.
    use lex_bytecode::op::Op;
    let src = "
fn sum_to(n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_to(n - 1, acc + n),
  }
}";
    let p = compile(src);
    let fused_in_body = p.functions.iter().flat_map(|f| f.code.iter()).any(|op|
        matches!(op, Op::LoadLocalEqIntConstJumpIfNot { .. }));
    assert!(fused_in_body, "slice 5 did not fire on sum_to's `0 =>` arm test");

    let mut vm = Vm::new(&p);
    // 1+2+3+4+5 = 15.
    let r = vm.call("sum_to", vec![Value::Int(5), Value::Int(0)]).unwrap();
    assert_eq!(r, Value::Int(15));
}

#[test]
fn slice5_runs_correctly_through_branch_not_taken() {
    // Edge: scrutinee matches the literal on the first arm → fused op
    // falls through to pc+4 (the arm body). Sum_to(0) should hit the
    // `0 => acc` arm immediately and return acc.
    let src = "
fn sum_to(n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_to(n - 1, acc + n),
  }
}";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("sum_to", vec![Value::Int(0), Value::Int(42)]).unwrap();
    assert_eq!(r, Value::Int(42));
}

#[test]
fn slice5_multi_arm_cascade_runs_correctly() {
    // Multiple Int-literal arms — each produces its own slice-5 fusion.
    // The 4-arm cascade should pick the right branch for each input.
    let src = "
fn classify(n :: Int) -> Int {
  match n {
    0 => 100,
    1 => 200,
    2 => 300,
    _ => 999,
  }
}";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("classify", vec![Value::Int(0)]).unwrap(), Value::Int(100));
    assert_eq!(vm.call("classify", vec![Value::Int(1)]).unwrap(), Value::Int(200));
    assert_eq!(vm.call("classify", vec![Value::Int(2)]).unwrap(), Value::Int(300));
    assert_eq!(vm.call("classify", vec![Value::Int(99)]).unwrap(), Value::Int(999));
}

#[test]
fn slice6_absorbs_match_scrutinee_dance() {
    // #461 slice 6: `match n { 0 => acc; _ => recurse }` compiles to
    // `LoadLocal(n) + StoreLocal(scrut) + slice5_fused{local_idx:
    // scrut, ...}`. Slice 6 collapses the leading LoadLocal+StoreLocal
    // into the fused arm-test, leaving them as tombstones. Verify
    // both that the fusion fires and that the runtime semantics
    // remain identical.
    use lex_bytecode::op::Op;
    let src = "
fn sum_to(n :: Int, acc :: Int) -> Int {
  match n {
    0 => acc,
    _ => sum_to(n - 1, acc + n),
  }
}";
    let p = compile(src);
    let fused = p.functions.iter().flat_map(|f| f.code.iter()).any(|op|
        matches!(op, Op::LoadLocalStoreEqIntConstJumpIfNot { .. }));
    assert!(fused, "slice 6 did not fire on sum_to");

    let mut vm = Vm::new(&p);
    // 1+2+3+4+5 = 15
    let r = vm.call("sum_to", vec![Value::Int(5), Value::Int(0)]).unwrap();
    assert_eq!(r, Value::Int(15));
}

#[test]
fn slice6_writes_dst_so_subsequent_arms_see_scrutinee() {
    // Critical correctness check for slice 6: the fused op MUST
    // mirror the original `StoreLocal(dst)` because the SECOND and
    // later arm tests in the same match still read `locals[dst]`.
    // A multi-arm cascade catches this — if the StoreLocal mirror
    // were dropped, every arm after the first would read garbage.
    let src = "
fn classify(n :: Int) -> Int {
  match n {
    0 => 100,
    1 => 200,
    2 => 300,
    3 => 400,
    _ => 999,
  }
}";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    // Every arm reads `locals[scrut]`; if slice 6 dropped the
    // StoreLocal mirror, arms past the first would see undefined
    // data and either return the wrong constant or hit the `_` arm.
    assert_eq!(vm.call("classify", vec![Value::Int(0)]).unwrap(), Value::Int(100));
    assert_eq!(vm.call("classify", vec![Value::Int(1)]).unwrap(), Value::Int(200));
    assert_eq!(vm.call("classify", vec![Value::Int(2)]).unwrap(), Value::Int(300));
    assert_eq!(vm.call("classify", vec![Value::Int(3)]).unwrap(), Value::Int(400));
    assert_eq!(vm.call("classify", vec![Value::Int(99)]).unwrap(), Value::Int(999));
}

#[test]
fn slice3_does_not_fire_across_a_jump_target() {
    // Safety check: if the second or third slot of the candidate
    // triple is a jump target, slice 3 must skip the fusion — a
    // jump landing on what looks like a `LoadLocal` is in fact a
    // live entry point, not an inert tombstone. `match` in the
    // body forces a JumpIfNot whose target sits between operations,
    // so we can construct a function where the LoadLocal+LoadLocal+
    // IntAdd triple straddles the arm boundary and verify no
    // fusion happens. Easier route: just check that running such
    // a function still produces the right answer (defensive — if
    // the safety check ever regressed, a wrong jump-target rewrite
    // would corrupt the stack and the call would either panic or
    // return junk).
    let src = "
fn pick(flag :: Int, a :: Int, b :: Int) -> Int {
  match flag {
    0 => a + b,
    _ => a,
  }
}";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("pick", vec![Value::Int(0), Value::Int(10), Value::Int(5)]).unwrap(), Value::Int(15));
    assert_eq!(vm.call("pick", vec![Value::Int(1), Value::Int(10), Value::Int(5)]).unwrap(), Value::Int(10));
}

#[test]
fn slice7_fuses_load_local_get_field_add() {
    // Pattern: a chain of `expr + r.field` reads. The first term
    // is a bare LoadLocal+GetField, the second-and-onward terms
    // each form a `[LoadLocal(r), GetField(field, ic_site),
    // IntAdd]` triple that slice 7 fuses. Verify at least one
    // fusion fires in the chain.
    use lex_bytecode::op::Op;
    let src = "
type R = { x :: Int, y :: Int, z :: Int }
fn sum_fields(r :: R) -> Int { r.x + r.y + r.z }
";
    let p = compile(src);
    let f = &p.functions[0];
    let fused_count = f.code.iter()
        .filter(|op| matches!(op, Op::LoadLocalGetFieldAdd { .. }))
        .count();
    assert!(fused_count >= 2,
        "expected ≥2 slice-7 fusions in y+z chain, got {fused_count}: {:?}",
        f.code);

    let mut vm = Vm::new(&p);
    let r = vm.call("sum_fields",
        vec![Value::record_dynamic({
            let mut m = IndexMap::new();
            m.insert("x".into(), Value::Int(10));
            m.insert("y".into(), Value::Int(20));
            m.insert("z".into(), Value::Int(30));
            m
        })]).unwrap();
    assert_eq!(r, Value::Int(60));
}

#[test]
fn slice7_works_on_stack_records_too() {
    // The slice-7 dispatch handler is polymorphic over Value::Record
    // and Value::StackRecord — the same path GetField uses. A
    // non-escaping record (lowered to AllocStackRecord by #464
    // step 2) chained through field-add should produce the right
    // answer.
    let src = r#"
        fn inline_sum() -> Int {
          let r := { a: 5, b: 11, c: 13 }
          r.a + r.b + r.c
        }
    "#;
    let p = compile(src);
    // Sanity: both lowerings should have fired in this function.
    use lex_bytecode::op::Op;
    let f = &p.functions[0];
    assert!(f.code.iter().any(|op| matches!(op, Op::AllocStackRecord { .. })),
        "expected escape lowering to fire");
    assert!(f.code.iter().any(|op| matches!(op, Op::LoadLocalGetFieldAdd { .. })),
        "expected slice 7 fusion to fire");

    let mut vm = Vm::new(&p);
    assert_eq!(vm.call("inline_sum", vec![]).unwrap(), Value::Int(29));
}

#[test]
fn slice7_does_not_fire_across_a_jump_target() {
    // Same safety story as slice 3/4: a `match`-arm body whose
    // entry point straddles the candidate triple's second or
    // third slot must skip the fusion. A regression would corrupt
    // the stack on the jump-into path. Check by running both arms
    // of a function shaped to put a GetField+IntAdd at an arm
    // boundary.
    let src = r#"
type R = { v :: Int }
fn pick(flag :: Int, r :: R) -> Int {
  match flag {
    0 => 100 + r.v,
    _ => r.v,
  }
}
"#;
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r1 = vm.call("pick", vec![Value::Int(0), Value::record_dynamic({
        let mut m = IndexMap::new(); m.insert("v".into(), Value::Int(7)); m
    })]).unwrap();
    assert_eq!(r1, Value::Int(107));
    let r2 = vm.call("pick", vec![Value::Int(1), Value::record_dynamic({
        let mut m = IndexMap::new(); m.insert("v".into(), Value::Int(7)); m
    })]).unwrap();
    assert_eq!(r2, Value::Int(7));
}

#[test]
fn record_field_access() {
    let src = "fn xof(r :: Record) -> Int { r.x }\n".replace(
        "Record",
        "{ x :: Int, y :: Int }",
    );
    let p = compile(&src);
    let mut vm = Vm::new(&p);
    let mut m = IndexMap::new();
    m.insert("x".into(), Value::Int(11));
    m.insert("y".into(), Value::Int(22));
    let r = vm.call("xof", vec![Value::record_dynamic(m)]).unwrap();
    assert_eq!(r, Value::Int(11));
}


#[test]
fn slice8_fuses_field_sub_and_computes_left_to_right() {
    // #461 slice 8: `acc - r.field` fuses to LoadLocalGetFieldSub.
    // The non-commutativity matters: `r.a - r.b - r.c` must be
    // (a - b) - c, not a - (b - c). With a=10, b=3, c=2 the correct
    // left-associated answer is 5.
    use lex_bytecode::op::Op;
    let src = "
type R = { a :: Int, b :: Int, c :: Int }
fn diff(r :: R) -> Int { r.a - r.b - r.c }
";
    let p = compile(src);
    let n = p.functions[0].code.iter()
        .filter(|op| matches!(op, Op::LoadLocalGetFieldSub { .. })).count();
    assert!(n >= 2, "expected ≥2 slice-8 sub fusions, got {n}: {:?}",
        p.functions[0].code);

    let mut vm = Vm::new(&p);
    let r = vm.call("diff", vec![Value::record_dynamic({
        let mut m = IndexMap::new();
        m.insert("a".into(), Value::Int(10));
        m.insert("b".into(), Value::Int(3));
        m.insert("c".into(), Value::Int(2));
        m
    })]).unwrap();
    assert_eq!(r, Value::Int(5));
}

#[test]
fn slice8_fuses_field_mul() {
    use lex_bytecode::op::Op;
    let src = "
type R = { a :: Int, b :: Int, c :: Int }
fn prod(r :: R) -> Int { r.a * r.b * r.c }
";
    let p = compile(src);
    let n = p.functions[0].code.iter()
        .filter(|op| matches!(op, Op::LoadLocalGetFieldMul { .. })).count();
    assert!(n >= 2, "expected ≥2 slice-8 mul fusions, got {n}: {:?}",
        p.functions[0].code);

    let mut vm = Vm::new(&p);
    let r = vm.call("prod", vec![Value::record_dynamic({
        let mut m = IndexMap::new();
        m.insert("a".into(), Value::Int(2));
        m.insert("b".into(), Value::Int(3));
        m.insert("c".into(), Value::Int(5));
        m
    })]).unwrap();
    assert_eq!(r, Value::Int(30));
}

#[test]
fn slice8_mixed_arith_chain() {
    // A chain mixing +, -, * over fields — exercises all three
    // slice-7/8 fused ops in one function and checks the combined
    // result. ((a + b) - c) gives 11+0... compute: a=4,b=6,c=3:
    // 4 + 6 = 10; 10 - 3 = 7; 7 * 2(=d) = 14.
    let src = "
type R = { a :: Int, b :: Int, c :: Int, d :: Int }
fn mix(r :: R) -> Int { r.a + r.b - r.c * r.d }
";
    // Note: Lex precedence — `*` binds tighter, so this is
    // r.a + r.b - (r.c * r.d). With a=4,b=6,c=3,d=2: 4+6-(6)=4.
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let r = vm.call("mix", vec![Value::record_dynamic({
        let mut m = IndexMap::new();
        m.insert("a".into(), Value::Int(4));
        m.insert("b".into(), Value::Int(6));
        m.insert("c".into(), Value::Int(3));
        m.insert("d".into(), Value::Int(2));
        m
    })]).unwrap();
    assert_eq!(r, Value::Int(4));
}

#[test]
fn slice8_sub_does_not_fire_across_a_jump_target() {
    // Jump-safety: same story as slice 3/4/7. A match-arm body whose
    // entry straddles the candidate triple must skip fusion.
    let src = "
type R = { v :: Int }
fn pick(flag :: Int, r :: R) -> Int {
  match flag {
    0 => 100 - r.v,
    _ => r.v,
  }
}
";
    let p = compile(src);
    let mut vm = Vm::new(&p);
    let mk = || Value::record_dynamic({
        let mut m = IndexMap::new(); m.insert("v".into(), Value::Int(7)); m
    });
    assert_eq!(vm.call("pick", vec![Value::Int(0), mk()]).unwrap(), Value::Int(93));
    assert_eq!(vm.call("pick", vec![Value::Int(1), mk()]).unwrap(), Value::Int(7));
}
