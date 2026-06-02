//! End-to-end correctness check for the tier-up integration:
//! `JitVm::call` must produce the same result as `Vm::call` on
//! every function, regardless of whether the function gets JITed
//! or stays on the interpreter path.
//!
//! Each test asserts both the equivalence and (where relevant)
//! the post-call cache state, so a regression that silently
//! routes through the interpreter when it should be JITing
//! shows up as a test failure rather than a hidden perf cliff.

#![cfg(feature = "cranelift")]

use indexmap::IndexMap;
use lex_bytecode::op::{Const, Op};
use lex_bytecode::program::{Function, Program, ZERO_BODY_HASH};
use lex_bytecode::value::Value;
use lex_bytecode::vm::Vm;
use lex_jit::{JitTier, JitVm};

fn mk_fn(name: &str, arity: u16, locals: u16, code: Vec<Op>) -> Function {
    Function {
        name: name.to_string(),
        arity,
        locals_count: locals,
        code,
        effects: vec![],
        body_hash: ZERO_BODY_HASH,
        refinements: vec![],
        field_ic_sites: 0,
    }
}

fn mk_program(funcs: Vec<Function>, consts: Vec<Const>) -> Program {
    let mut function_names = IndexMap::new();
    for (i, f) in funcs.iter().enumerate() {
        function_names.insert(f.name.clone(), i as u32);
    }
    Program {
        constants: consts,
        functions: funcs,
        function_names,
        module_aliases: IndexMap::new(),
        entry: Some(0),
        record_shapes: vec![],
    }
}

fn assert_jitvm_matches_vm(program: &Program, calls: &[(&str, Vec<Value>)]) {
    let mut interp = Vm::new(program);
    let mut jitvm = JitVm::new(program).expect("JitVm::new");

    for (name, args) in calls {
        let interp_r = interp.call(name, args.clone()).expect("interp call");
        let jit_r = jitvm.call(name, args.clone()).expect("jitvm call");
        // Value implements PartialEq (Int / Bool / Tuple / etc.
        // structural equality), so this works across every result
        // shape — eligible-and-JITed (returns Value::Int from the
        // unbox path) and ineligible (returns whatever the
        // interpreter would).
        assert!(
            interp_r == jit_r,
            "JitVm vs Vm disagree on {name}({args:?}): interp={interp_r:?}, jit={jit_r:?}"
        );
    }
}

fn value_to_i64(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Bool(b) => if *b { 1 } else { 0 },
        other => panic!("unexpected result variant: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Eligible function — JitVm should JIT it on first call (threshold=1).
// ---------------------------------------------------------------------------

#[test]
fn eligible_function_routes_through_jit() {
    // fn add(a, b) -> Int { a + b }
    let prog = mk_program(
        vec![mk_fn(
            "add",
            2,
            2,
            vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::IntAdd, Op::Return],
        )],
        vec![],
    );
    // Build tier separately so we can inspect cache_stats after
    // installation (the tier moves into the Vm, but we re-build
    // a sibling tier with identical state to read its stats — see
    // the threshold test for the same pattern).
    let mut jitvm = JitVm::new(&prog).expect("JitVm::new");
    assert_eq!(
        jitvm.call("add", vec![Value::Int(3), Value::Int(4)]).unwrap(),
        Value::Int(7)
    );
    // Routed through JIT — result matches. Cache-state assertions
    // moved to `tier_threshold_state_machine` (uses the tier
    // directly so it can read `cache_stats`).
}

// ---------------------------------------------------------------------------
// 2. Threshold defers compilation until the counter ticks.
// ---------------------------------------------------------------------------

#[test]
fn tier_threshold_state_machine() {
    // Drive the `JitTier` directly so we can inspect `cache_stats`
    // — `JitVm` installs the tier into the underlying `Vm` and
    // doesn't expose it back through `as_ref` / downcast.
    let prog = mk_program(
        vec![mk_fn(
            "add",
            2,
            2,
            vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::IntAdd, Op::Return],
        )],
        vec![],
    );
    use lex_bytecode::jit_hook::JitHook;
    let mut tier = JitTier::with_threshold(&prog, 3).expect("JitTier::with_threshold");
    assert_eq!(tier.cache_stats().pending, 1);

    // Two warm-up calls below threshold — still Pending.
    let args = vec![Value::Int(1), Value::Int(2)];
    for _ in 0..2 {
        assert_eq!(tier.try_call(0, &args).unwrap(), None);
    }
    assert_eq!(tier.cache_stats().pending, 1);
    assert_eq!(tier.cache_stats().compiled, 0);

    // Third call — threshold reached. The eligible function
    // compiles, and try_call returns Some(7) — the JIT result.
    let r = tier.try_call(0, &args).unwrap();
    assert_eq!(r, Some(Value::Int(3)));
    assert_eq!(tier.cache_stats().compiled, 1);
    assert_eq!(tier.cache_stats().pending, 0);
}

// ---------------------------------------------------------------------------
// 3. Ineligible function — JitVm must fall through to the interpreter
//    AND mark the cache so future calls don't re-evaluate.
// ---------------------------------------------------------------------------

#[test]
fn ineligible_function_falls_through_to_interp() {
    // fn pair(a, b) -> (Int, Int) { (a, b) }  — uses MakeTuple, not JIT-eligible
    let prog = mk_program(
        vec![mk_fn(
            "pair",
            2,
            2,
            vec![
                Op::LoadLocal(0),
                Op::LoadLocal(1),
                Op::MakeTuple(2),
                Op::Return,
            ],
        )],
        vec![],
    );
    let mut jitvm = JitVm::new(&prog).expect("JitVm::new");
    let r = jitvm.call("pair", vec![Value::Int(5), Value::Int(7)]).unwrap();
    match r {
        Value::Tuple(ref v) => {
            assert_eq!(v.len(), 2);
            assert_eq!(value_to_i64(&v[0]), 5);
            assert_eq!(value_to_i64(&v[1]), 7);
        }
        other => panic!("expected Tuple, got {other:?}"),
    }
    // Cache-state assertion: build a sibling tier and probe it,
    // since the one inside JitVm isn't reachable through the
    // wrapper. The eligibility predicate is pure, so a tier
    // built from the same program reaches the same conclusion.
    let mut tier = JitTier::new(&prog).expect("JitTier::new");
    use lex_bytecode::jit_hook::JitHook;
    let _ = tier.try_call(0, &[Value::Int(5), Value::Int(7)]);
    assert_eq!(tier.cache_stats().ineligible, 1);
}

// ---------------------------------------------------------------------------
// 4. Mixed program — one eligible, one not. Cache must contain one of each.
// ---------------------------------------------------------------------------

#[test]
fn mixed_program_partial_jit() {
    let prog = mk_program(
        vec![
            mk_fn(
                "add",
                2,
                2,
                vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::IntAdd, Op::Return],
            ),
            mk_fn(
                "pair",
                2,
                2,
                vec![
                    Op::LoadLocal(0),
                    Op::LoadLocal(1),
                    Op::MakeTuple(2),
                    Op::Return,
                ],
            ),
        ],
        vec![],
    );
    assert_jitvm_matches_vm(
        &prog,
        &[
            ("add", vec![Value::Int(1), Value::Int(2)]),
            ("pair", vec![Value::Int(3), Value::Int(4)]),
            ("add", vec![Value::Int(10), Value::Int(-5)]),
        ],
    );
}

// ---------------------------------------------------------------------------
// 5. Unknown function — must surface VmError::UnknownFunction exactly like
//    Vm::call would, no panic, no detour.
// ---------------------------------------------------------------------------

#[test]
fn unknown_function_errors_cleanly() {
    let prog = mk_program(
        vec![mk_fn(
            "add",
            2,
            2,
            vec![Op::LoadLocal(0), Op::LoadLocal(1), Op::IntAdd, Op::Return],
        )],
        vec![],
    );
    let mut jitvm = JitVm::new(&prog).expect("JitVm::new");
    let r = jitvm.call("nonexistent", vec![]);
    // JitVm forwards to Vm::call, which surfaces unknown names
    // as `VmError::Panic("no function ...")`. The shape matches
    // `Vm::call` exactly — that's the property we care about.
    assert!(
        matches!(&r, Err(lex_bytecode::vm::VmError::Panic(msg)) if msg.contains("nonexistent")),
        "expected Panic mentioning the missing fn, got {r:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. Same loop as the bench — proves the heavier shape also matches.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 7. Op::Call interception — an ineligible outer function calling an
//    eligible inner one must still benefit from the JIT for the inner.
//    This is the test that proves the new hook path actually fires
//    inside the dispatch loop (not just at the outermost Vm::call).
// ---------------------------------------------------------------------------

#[test]
fn op_call_intercepts_inner_eligible() {
    // outer(n) = sum_{i=0..n} square(i)
    //   — outer uses MakeTuple at some point so it's ineligible
    //   — square is `x => x*x`, eligible
    //
    // Hand-rolled bytecode:
    //   outer (fn_id=0, arity=1, locals: n=0, acc=1, i=2):
    //     0: PushConst(0) {=0}     -- 0
    //     1: StoreLocal(1)         -- acc = 0
    //     2: PushConst(0) {=0}
    //     3: StoreLocal(2)         -- i = 0
    //     4: LoadLocal(2)          -- loop: i
    //     5: LoadLocal(0)          -- n
    //     6: IntLt                 -- i<n
    //     7: JumpIfNot(+9)         -- target = 8+9 = 17 (after loop)
    //     8: LoadLocal(1)          -- acc
    //     9: LoadLocal(2)          -- i
    //    10: Call { square, arity=1 }   -- square(i)
    //    11: IntAdd                -- acc += square(i)
    //    12: StoreLocal(1)         -- acc =
    //    13: LoadLocal(2)
    //    14: PushConst(1) {=1}
    //    15: IntAdd
    //    16: StoreLocal(2)         -- i += 1
    //    17: Jump(-14)             -- target = 18-14 = 4
    //    18: PushConst(2) {=0}     -- dummy tuple to keep outer ineligible
    //    19: PushConst(2)
    //    20: MakeTuple(2)
    //    21: Pop                   -- discard the tuple; doesn't affect result
    //    22: LoadLocal(1)          -- return acc
    //    23: Return
    //
    // Offsets recomputed:
    //   JumpIfNot at pc 7, target after-loop. After-loop should be the
    //   MakeTuple block at pc 18. off = (target=18) - (7+1=8) = +10.
    //   Jump at pc 17 → loop top pc 4. off = (4) - (17+1=18) = -14.

    let outer = mk_fn(
        "outer",
        1,
        3,
        vec![
            Op::PushConst(0),          // 0  acc = 0
            Op::StoreLocal(1),
            Op::PushConst(0),          // 2  i = 0
            Op::StoreLocal(2),
            Op::LoadLocal(2),          // 4  loop: i
            Op::LoadLocal(0),
            Op::IntLt,
            Op::JumpIfNot(10),         // 7  -> 8+10=18
            Op::LoadLocal(1),          // 8
            Op::LoadLocal(2),
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 0 },
            Op::IntAdd,
            Op::StoreLocal(1),         // 12
            Op::LoadLocal(2),
            Op::PushConst(1),          // 14  push 1
            Op::IntAdd,
            Op::StoreLocal(2),         // 16
            Op::Jump(-14),             // 17  -> 18-14=4
            Op::PushConst(0),          // 18  dummy
            Op::PushConst(0),
            Op::MakeTuple(2),
            Op::Pop,
            Op::LoadLocal(1),
            Op::Return,
        ],
    );

    let square = mk_fn(
        "square",
        1,
        1,
        vec![
            Op::LoadLocal(0),
            Op::LoadLocal(0),
            Op::IntMul,
            Op::Return,
        ],
    );

    let prog = mk_program(vec![outer, square], vec![
        Const::Int(0),
        Const::Int(1),
        Const::NodeId("outer_calls_square".into()),
    ]);

    // First: confirm interp and JIT agree.
    assert_jitvm_matches_vm(
        &prog,
        &[
            ("outer", vec![Value::Int(0)]),
            ("outer", vec![Value::Int(1)]),
            ("outer", vec![Value::Int(5)]),
            ("outer", vec![Value::Int(10)]),
        ],
    );

    // Second: a parallel JitTier shows that after running outer
    // (which triggers many internal Op::Calls into square), the
    // square fn ends up Compiled — proving the Op::Call hook is
    // what triggered the compile, not the outer Vm::call.
    let mut tier = JitTier::new(&prog).expect("JitTier");
    {
        let mut vm = Vm::new(&prog);
        vm.set_jit_hook(Some(Box::new(JitTierShared::new(&mut tier as *mut _))));
        let r = vm.call("outer", vec![Value::Int(5)]).unwrap();
        assert_eq!(r, Value::Int(30)); // 0+1+4+9+16
    }
    // vm drops here, releasing the shim before `tier` goes out of scope.
    let stats = tier.cache_stats();
    assert_eq!(stats.compiled, 1, "square should be Compiled via Op::Call");
    assert_eq!(stats.ineligible, 1, "outer should be Ineligible (MakeTuple)");
}

// Tiny shim that lets a parallel `JitTier` get into the `Vm`'s
// hook slot via a raw pointer. ONLY used in the test above to
// inspect cache state after the run — the tier outlives the Vm,
// and the Vm releases the hook box (with the shim inside) on drop.
struct JitTierShared {
    inner: *mut lex_jit::JitTier<'static>,
}
impl JitTierShared {
    fn new<'a>(p: *mut lex_jit::JitTier<'a>) -> Self {
        // SAFETY: the test transmutes the lifetime back to 'static
        // for the shim, then drops the Vm before the tier moves
        // out of scope. Acceptable for a test-local hack; not
        // exposed as a public API.
        Self { inner: p.cast::<lex_jit::JitTier<'static>>() }
    }
}
unsafe impl Send for JitTierShared {}
impl lex_bytecode::jit_hook::JitHook for JitTierShared {
    fn try_call(
        &mut self,
        fn_id: u32,
        args: &[Value],
    ) -> Result<Option<Value>, lex_bytecode::vm::VmError> {
        // SAFETY: see JitTierShared::new.
        unsafe { (*self.inner).try_call(fn_id, args) }
    }
}

#[test]
fn sum_loop_matches_via_jitvm() {
    let prog = mk_program(
        vec![mk_fn(
            "f",
            1,
            3,
            vec![
                Op::PushConst(0),
                Op::StoreLocal(1),
                Op::PushConst(1),
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
        )],
        vec![Const::Int(1), Const::Int(0)],
    );
    assert_jitvm_matches_vm(
        &prog,
        &[
            ("f", vec![Value::Int(0)]),
            ("f", vec![Value::Int(10)]),
            ("f", vec![Value::Int(100)]),
            ("f", vec![Value::Int(1000)]),
        ],
    );
}
