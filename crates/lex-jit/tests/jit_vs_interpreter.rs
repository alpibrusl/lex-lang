//! End-to-end correctness check for the MVP JIT: every function
//! we JIT must produce the same i64 result as the bytecode
//! interpreter on the same inputs.
//!
//! Each test builds a hand-crafted `Function` + `Program` (no
//! source-level compiler involved — these are bytecode literals),
//! runs the interpreter via `Vm::call`, runs the JIT via
//! `JitContext::compile(...).call(...)`, and asserts the results
//! match across a battery of inputs.

#![cfg(feature = "cranelift")]

use indexmap::IndexMap;
use lex_bytecode::op::{Const, Op};
use lex_bytecode::program::{Function, Program, ZERO_BODY_HASH};
use lex_bytecode::value::Value;
use lex_bytecode::vm::Vm;
use lex_jit::{is_jit_eligible, JitContext};

fn mk_program(name: &str, arity: u16, locals: u16, code: Vec<Op>, consts: Vec<Const>) -> Program {
    let mut function_names = IndexMap::new();
    function_names.insert(name.to_string(), 0);
    Program {
        constants: consts,
        functions: vec![Function {
            name: name.to_string(),
            arity,
            locals_count: locals,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        }],
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![],
    }
}

fn run_interp(program: &Program, args: Vec<i64>) -> i64 {
    let mut vm = Vm::new(program);
    let arg_values: Vec<Value> = args.into_iter().map(Value::Int).collect();
    let r = vm.call("f", arg_values).expect("interpreter run");
    match r {
        Value::Int(n) => n,
        Value::Bool(b) => if b { 1 } else { 0 },
        other => panic!("unexpected return type: {other:?}"),
    }
}

fn jit_and_call(program: &Program, args: &[i64]) -> i64 {
    let f = &program.functions[0];
    assert!(
        is_jit_eligible(f, &program.constants),
        "function not JIT-eligible — fix the test or extend the JIT"
    );
    let mut ctx = JitContext::new().expect("JitContext init");
    let jitted = ctx.compile(f, &program.constants).expect("JIT compile");
    unsafe { jitted.call(args) }
}

fn assert_jit_matches(program: &Program, inputs: &[Vec<i64>]) {
    for args in inputs {
        let interp = run_interp(program, args.clone());
        let jit = jit_and_call(program, args);
        assert_eq!(
            interp, jit,
            "JIT vs interpreter disagree on args {args:?}: interp={interp}, jit={jit}"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Straight-line int arithmetic
// ---------------------------------------------------------------------------

#[test]
fn add_two_ints() {
    // fn f(a, b) -> Int { a + b }
    let p = mk_program(
        "f",
        2,
        2,
        vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::IntAdd, Op::Return],
        vec![],
    );
    assert_jit_matches(
        &p,
        &[
            vec![3, 4],
            vec![100, -50],
            vec![0, 0],
            vec![i64::MAX, 0],
            vec![-1, 1],
        ],
    );
}

#[test]
fn polynomial_mul_add() {
    // fn f(a, b, c) -> Int { a * b + c }
    let p = mk_program(
        "f",
        3,
        3,
        vec![
            Op::LoadLocal(0),
            Op::LoadLocal(1),
            Op::IntMul,
            Op::LoadLocal(2),
            Op::IntAdd,
            Op::Return,
        ],
        vec![],
    );
    assert_jit_matches(
        &p,
        &[
            vec![2, 3, 4],   // 2*3 + 4 = 10
            vec![0, 99, 7],  // 0*99 + 7 = 7
            vec![-2, 3, 10], // -6 + 10 = 4
        ],
    );
}

#[test]
fn use_const_pool() {
    // fn f(a) -> Int { a + 42 }
    let p = mk_program(
        "f",
        1,
        1,
        vec![
            Op::LoadLocal(0),
            Op::PushConst(0),
            Op::IntAdd,
            Op::Return,
        ],
        vec![Const::Int(42)],
    );
    assert_jit_matches(&p, &[vec![0], vec![8], vec![-100], vec![i64::MAX - 42]]);
}

#[test]
fn store_and_reuse_local() {
    // fn f(a, b) -> Int {
    //   let t = a + b;      // local 2
    //   t * t
    // }
    let p = mk_program(
        "f",
        2,
        3,
        vec![
            Op::LoadLocal(0),
            Op::LoadLocal(1),
            Op::IntAdd,
            Op::StoreLocal(2),
            Op::LoadLocal(2),
            Op::LoadLocal(2),
            Op::IntMul,
            Op::Return,
        ],
        vec![],
    );
    assert_jit_matches(&p, &[vec![3, 4], vec![0, 0], vec![-2, 5]]);
}

#[test]
fn int_neg_div_mod() {
    // fn f(a, b) -> Int { (-a) / b + (a % b) }
    let p = mk_program(
        "f",
        2,
        2,
        vec![
            Op::LoadLocal(0),
            Op::IntNeg,
            Op::LoadLocal(1),
            Op::IntDiv,
            Op::LoadLocal(0),
            Op::LoadLocal(1),
            Op::IntMod,
            Op::IntAdd,
            Op::Return,
        ],
        vec![],
    );
    assert_jit_matches(&p, &[vec![10, 3], vec![100, 7], vec![-15, 4]]);
}

// ---------------------------------------------------------------------------
// 2. Boolean ops + comparisons
// ---------------------------------------------------------------------------

#[test]
fn int_lt_then_extend() {
    // fn f(a, b) -> Bool { a < b }
    // The interpreter returns Bool; the JIT returns 0/1 as i64.
    // run_interp coerces Bool→0/1, so they match.
    let p = mk_program(
        "f",
        2,
        2,
        vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::IntLt, Op::Return],
        vec![],
    );
    assert_jit_matches(
        &p,
        &[vec![1, 2], vec![5, 5], vec![10, 3], vec![-1, 0]],
    );
}

#[test]
fn bool_logic() {
    // fn f(a, b) -> Bool { (a < b) && !(a == 0) }
    let p = mk_program(
        "f",
        2,
        2,
        vec![
            // a < b
            Op::LoadLocal(0),
            Op::LoadLocal(1),
            Op::IntLt,
            // a == 0
            Op::LoadLocal(0),
            Op::PushConst(0),
            Op::IntEq,
            Op::BoolNot,
            // and
            Op::BoolAnd,
            Op::Return,
        ],
        vec![Const::Int(0)],
    );
    assert_jit_matches(
        &p,
        &[vec![1, 2], vec![0, 1], vec![-5, 3], vec![5, 1]],
    );
}

// ---------------------------------------------------------------------------
// 3. Control flow — forward conditional jump
// ---------------------------------------------------------------------------

#[test]
fn abs_via_jumpifnot() {
    // fn f(a) -> Int { if a < 0 { -a } else { a } }
    //
    //   0: LoadLocal(0)
    //   1: PushConst(0)     -- 0
    //   2: IntLt             -- (a<0)
    //   3: JumpIfNot(+3)     -- target = (3+1)+3 = 7 (else arm)
    //   4: LoadLocal(0)      -- then arm
    //   5: IntNeg
    //   6: Jump(+1)          -- target = (6+1)+1 = 8 (return)
    //   7: LoadLocal(0)      -- else arm
    //   8: Return
    let p = mk_program(
        "f",
        1,
        1,
        vec![
            Op::LoadLocal(0),
            Op::PushConst(0),
            Op::IntLt,
            Op::JumpIfNot(3),
            Op::LoadLocal(0),
            Op::IntNeg,
            Op::Jump(1),
            Op::LoadLocal(0),
            Op::Return,
        ],
        vec![Const::Int(0)],
    );
    assert_jit_matches(&p, &[vec![5], vec![-7], vec![0], vec![i64::MIN + 1]]);
}

#[test]
fn max_via_brif() {
    // fn f(a, b) -> Int { if a < b { b } else { a } }
    //
    //   0: LoadLocal(0)
    //   1: LoadLocal(1)
    //   2: IntLt
    //   3: JumpIfNot(+3)     -- target = 4+3 = 7 (else arm)
    //   4: LoadLocal(1)       -- then arm
    //   5: Jump(+2)           -- target = 6+2 = 8 (return)
    //   6: IntAdd             -- dead-op tombstone, unreachable
    //   7: LoadLocal(0)       -- else arm
    //   8: Return
    let p = mk_program(
        "f",
        2,
        2,
        vec![
            Op::LoadLocal(0),
            Op::LoadLocal(1),
            Op::IntLt,
            Op::JumpIfNot(3),
            Op::LoadLocal(1),
            Op::Jump(2),
            Op::IntAdd,
            Op::LoadLocal(0),
            Op::Return,
        ],
        vec![],
    );
    assert_jit_matches(
        &p,
        &[vec![1, 2], vec![5, 3], vec![0, 0], vec![-5, -10]],
    );
}

// ---------------------------------------------------------------------------
// 4. Control flow — loop (backward jump)
// ---------------------------------------------------------------------------

#[test]
fn sum_from_one_to_n_via_loop() {
    // fn f(n) -> Int {
    //   let i = 1
    //   let acc = 0
    //   loop:
    //     if !(i <= n) goto end
    //     acc = acc + i
    //     i = i + 1
    //     goto loop
    //   end:
    //     return acc
    //
    // Locals: 0 = n (arg), 1 = i, 2 = acc.
    //
    //   0: PushConst(1) {=1}
    //   1: StoreLocal(1)           i = 1
    //   2: PushConst(0) {=0}
    //   3: StoreLocal(2)           acc = 0
    //   4: LoadLocal(1)            loop:  i
    //   5: LoadLocal(0)                   i n
    //   6: IntLe                          (i<=n)
    //   7: JumpIfNot(+7)           -> pc 15 (end)
    //   8: LoadLocal(2)            acc
    //   9: LoadLocal(1)            acc i
    //  10: IntAdd                  (acc+i)
    //  11: StoreLocal(2)           acc=
    //  12: LoadLocal(1)            i
    //  13: PushConst(1)            i 1
    //  14: IntAdd                  (i+1)
    //  15: StoreLocal(1)           i =
    //  16: Jump(-13)               -> pc 4
    //  17: LoadLocal(2)            end:
    //  18: Return
    //
    // Re-check offsets:
    //   JumpIfNot at pc 7 → target 8+off. We want end = pc 17.
    //     off = 17 - 8 = +9.
    //   Jump at pc 16 → target 17+off. We want pc 4.
    //     off = 4 - 17 = -13.
    //
    // (My count above had StoreLocal at pc 15 then Jump at pc 16 —
    // matching the spec.)
    let p = mk_program(
        "f",
        1,
        3,
        vec![
            Op::PushConst(0), // const slot 0 = 1
            Op::StoreLocal(1),
            Op::PushConst(1), // const slot 1 = 0
            Op::StoreLocal(2),
            Op::LoadLocal(1),
            Op::LoadLocal(0),
            Op::IntLe,
            Op::JumpIfNot(9),
            Op::LoadLocal(2),
            Op::LoadLocal(1),
            Op::IntAdd,
            Op::StoreLocal(2),
            Op::LoadLocal(1),
            Op::PushConst(0),
            Op::IntAdd,
            Op::StoreLocal(1),
            Op::Jump(-13),
            Op::LoadLocal(2),
            Op::Return,
        ],
        vec![Const::Int(1), Const::Int(0)],
    );
    assert_jit_matches(
        &p,
        &[vec![0], vec![1], vec![5], vec![10], vec![100], vec![1000]],
    );
}

// ---------------------------------------------------------------------------
// 5. Eligibility gate
// ---------------------------------------------------------------------------

#[test]
fn rejects_unsupported_op_via_eligibility() {
    // A function with `MakeTuple` is outside the MVP scope — the
    // gate must say so without trying to compile.
    let f = lex_bytecode::program::Function {
        name: "f".into(),
        arity: 1,
        locals_count: 1,
        code: vec![
            Op::LoadLocal(0),
            Op::LoadLocal(0),
            Op::MakeTuple(2),
            Op::Return,
        ],
        effects: vec![],
        body_hash: ZERO_BODY_HASH,
        refinements: vec![],
        field_ic_sites: 0,
    };
    assert!(!is_jit_eligible(&f, &[]));
}

#[test]
fn rejects_unsupported_const_via_eligibility() {
    // PushConst(Str) is unsupported — Strs can't be unboxed to i64.
    let f = lex_bytecode::program::Function {
        name: "f".into(),
        arity: 0,
        locals_count: 0,
        code: vec![Op::PushConst(0), Op::Return],
        effects: vec![],
        body_hash: ZERO_BODY_HASH,
        refinements: vec![],
        field_ic_sites: 0,
    };
    assert!(!is_jit_eligible(&f, &[Const::Str("nope".into())]));
}

#[test]
fn rejects_overlarge_arity() {
    let f = lex_bytecode::program::Function {
        name: "f".into(),
        arity: 7,
        locals_count: 7,
        code: vec![Op::LoadLocal(0), Op::Return],
        effects: vec![],
        body_hash: ZERO_BODY_HASH,
        refinements: vec![],
        field_ic_sites: 0,
    };
    assert!(!is_jit_eligible(&f, &[]));
}
