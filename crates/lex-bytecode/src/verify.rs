//! Bytecode stack-depth verifier — the third `--strict` check from #347 A2.
//!
//! Walks every function's instruction stream, tracking the abstract stack
//! depth through each opcode and branch. Reports a `StackError` when two
//! paths into the same program counter carry different depths, which would
//! mean a prior match arm leaked (or over-consumed) values and left the
//! stack in an inconsistent state for subsequent arms.
//!
//! The check is lightweight: it is a single linear pass with a small
//! worklist. No allocation beyond `Vec` is needed.
//!
//! # Known sound over-approximation
//!
//! `Return` and `Panic` terminate the function; their successors are not
//! added to the worklist. `TailCall` is treated like `Return`. This means
//! dead code after a `Return` / `Panic` is not checked — intentional.

use crate::op::Op;
use crate::program::Function;

#[derive(Debug, Clone, PartialEq)]
pub struct StackError {
    pub fn_name: String,
    pub pc: usize,
    pub depth_a: i32,
    pub depth_b: i32,
}

impl std::fmt::Display for StackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stack depth mismatch in `{}` at pc {}: path A leaves depth {}, path B leaves depth {}",
            self.fn_name, self.pc, self.depth_a, self.depth_b
        )
    }
}

/// Verify all functions in a slice. Returns one error per inconsistent
/// merge point found.
pub fn verify_program(functions: &[Function]) -> Vec<StackError> {
    let mut errors = Vec::new();
    for func in functions {
        verify_function(func, &mut errors);
    }
    errors
}

/// Verify a single function. Appends to `errors`.
pub fn verify_function(func: &Function, errors: &mut Vec<StackError>) {
    let n = func.code.len();
    if n == 0 {
        return;
    }

    // `depths[pc]` = known stack depth at that pc, or `None` if not yet visited.
    let mut depths: Vec<Option<i32>> = vec![None; n];

    // Worklist: (pc, stack_depth_on_entry_to_this_instruction)
    let mut worklist: Vec<(usize, i32)> = vec![(0, 0)];

    while let Some((pc, depth)) = worklist.pop() {
        if pc >= n {
            continue;
        }

        // Merge-point check.
        if let Some(prev) = depths[pc] {
            if prev != depth {
                errors.push(StackError {
                    fn_name: func.name.clone(),
                    pc,
                    depth_a: prev,
                    depth_b: depth,
                });
            }
            // Already processed from this depth (or recorded mismatch).
            continue;
        }
        depths[pc] = Some(depth);

        let op = &func.code[pc];
        let delta = stack_delta(op);
        let next_depth = depth + delta;

        match op {
            // Unconditional jumps: only the target is a successor.
            Op::Jump(off) => {
                let target = (pc as i32 + 1 + off) as usize;
                worklist.push((target, next_depth));
            }
            // Conditional jumps: fall-through and jump target are both successors.
            // Note: JumpIf / JumpIfNot pop the Bool before branching, so `delta`
            // already accounts for that (-1). Both successors start at next_depth.
            Op::JumpIf(off) | Op::JumpIfNot(off) => {
                let target = (pc as i32 + 1 + off) as usize;
                worklist.push((pc + 1, next_depth));
                worklist.push((target, next_depth));
            }
            // Terminators: no successors.
            Op::Return | Op::TailCall { .. } | Op::Panic(_) => {}
            // Slice-2 superinstruction (#461) owns 4 slots: the fused
            // op + 3 tombstones (original PushConst + IntAdd +
            // StoreLocal). The trailing tombstones' deltas don't
            // cancel (+1, -1, -1 = -1), so we can't let the verifier
            // walk them as live — it'd drift the depth at pc+4 vs the
            // pre-fusion form. Skip directly to pc+4 with the
            // unfused-equivalent depth.
            Op::LoadLocalAddIntConstStoreLocal { .. } => {
                worklist.push((pc + 4, next_depth));
            }
            // Slice-5 superinstructions (#461) — jump-aware fusion of
            // `LoadLocal + LoadLocal|PushConst + IntLt + JumpIfNot`.
            // 4-slot window like slice 2 but with TWO successors
            // (fall-through and branch target). Tombstones' deltas
            // (+1 LoadLocal/PushConst, -1 IntLt, -1 JumpIfNot = -1
            // total) don't cancel — skip past in the same shape as
            // slice 2's worklist override. The branch target's
            // offset is relative to the JumpIfNot's `pc + 1`, which
            // in the fused position is `pc + 4`.
            Op::LoadLocalEqIntConstJumpIfNot { jump_offset, .. } => {
                let target = (pc as i32 + 4 + jump_offset) as usize;
                worklist.push((pc + 4, next_depth));
                worklist.push((target, next_depth));
            }
            // Slice 6 owns a 6-slot window (this op + 5 tombstones).
            // Same two-successor shape as slice 5 but with `pc + 6`
            // arithmetic. Net stack delta is 0 — original was
            // LoadLocal + StoreLocal + slice5 = +1 + -1 + 0.
            Op::LoadLocalStoreEqIntConstJumpIfNot { jump_offset, .. } => {
                let target = (pc as i32 + 6 + jump_offset) as usize;
                worklist.push((pc + 6, next_depth));
                worklist.push((target, next_depth));
            }
            // All other ops: single sequential successor.
            _ => {
                worklist.push((pc + 1, next_depth));
            }
        }
    }
}

/// Returns the net change in stack depth caused by `op`.
///
/// Positive = pushes more than it pops.
/// Negative = pops more than it pushes.
fn stack_delta(op: &Op) -> i32 {
    match op {
        // Stack manipulation
        Op::PushConst(_)  =>  1,
        Op::Pop           => -1,
        Op::Dup           =>  1,

        // Locals
        Op::LoadLocal(_)  =>  1,
        Op::StoreLocal(_) => -1,

        // Record / tuple / list construction
        Op::MakeRecord { field_count, .. } => -(*field_count as i32) + 1,
        // #464 step 2: same stack-effect shape as MakeRecord (pops
        // field_count, pushes 1). The verifier doesn't need to know
        // about the stack-record arena — it walks bytecode shape only.
        Op::AllocStackRecord { field_count, .. } => -(*field_count as i32) + 1,
        Op::MakeTuple(n)  => -(*n as i32) + 1,
        Op::MakeList(n)   => -(*n as i32) + 1,
        Op::MakeVariant { arity, .. } => -(*arity as i32) + 1,

        // Field/element access: pop 1, push 1
        Op::GetField { .. } => 0,
        Op::GetElem(_)     => 0,
        Op::GetListElem(_) => 0,
        Op::GetListLen     => 0,

        // Variant ops: pop 1, push 1
        Op::TestVariant(_)  => 0,
        Op::GetVariant(_)   => 0,
        Op::GetVariantArg(_)=> 0,

        // Binary list ops: pop 2, push 1
        Op::ListAppend      => -1,
        Op::GetListElemDyn  => -1,

        // Jumps: delta handled in the control-flow logic above; use 0 here
        // so that next_depth = depth + 0 is the "effective post-instruction depth"
        // before branching. The successor depths are added by the control-flow arms.
        Op::Jump(_) | Op::JumpIf(_) | Op::JumpIfNot(_) => {
            // JumpIf/JumpIfNot pop the Bool.
            match op {
                Op::JumpIf(_) | Op::JumpIfNot(_) => -1,
                _ => 0,
            }
        }

        // Calls: pop arity args, push 1 result
        Op::Call { arity, .. }       => -(*arity as i32) + 1,
        Op::TailCall { arity, .. }   => -(*arity as i32) + 1,
        Op::CallClosure { arity, .. }=> -(*arity as i32 + 1) + 1,  // also pops closure
        Op::EffectCall { arity, .. } => -(*arity as i32) + 1,

        // Closure construction: pop captures, push closure
        Op::MakeClosure { capture_count, .. } => -(*capture_count as i32) + 1,

        // Higher-order ops: pop list + fn, push result list
        Op::SortByKey { .. }  => -1,
        Op::ParallelMap { .. }=> -1,

        // Terminators
        Op::Return  => -1,  // pop return value
        Op::Panic(_)=>  0,  // does not matter (no successor)

        // Arithmetic / comparison — all binary except Neg/Not
        Op::IntAdd | Op::IntSub | Op::IntMul | Op::IntDiv | Op::IntMod => -1,
        Op::IntEq  | Op::IntLt  | Op::IntLe  => -1,
        Op::IntNeg => 0,

        Op::FloatAdd | Op::FloatSub | Op::FloatMul | Op::FloatDiv => -1,
        Op::FloatEq  | Op::FloatLt  | Op::FloatLe  => -1,
        Op::FloatNeg => 0,

        Op::NumAdd | Op::NumSub | Op::NumMul | Op::NumDiv | Op::NumMod => -1,
        Op::NumEq  | Op::NumLt  | Op::NumLe  => -1,
        Op::NumNeg => 0,

        Op::BoolAnd | Op::BoolOr => -1,
        Op::BoolNot => 0,

        Op::StrConcat => -1,
        Op::StrLen    =>  0,
        Op::StrEq     => -1,
        Op::BytesLen  =>  0,
        Op::BytesEq   => -1,

        // Superinstructions (#461). The fused op contributes its
        // net delta (+1, same shape as a bare LoadLocal). The two
        // inert primitive ops the peephole leaves at pc+1 / pc+2
        // are walked as if live: their deltas (+1 PushConst, -1
        // IntAdd) cancel, so the depth at pc+3 matches what the
        // unfused sequence would have produced.
        Op::LoadLocalAddIntConst { .. } => 1,
        // Slice-2 fused op: src → dest with no net stack effect.
        // Tombstones at the next 3 slots are *not* walked (see the
        // control-flow successor logic in `verify_function`).
        Op::LoadLocalAddIntConstStoreLocal { .. } => 0,
        // Slice-3 fused op: LoadLocal(lhs) + LoadLocal(rhs) + IntAdd.
        // Net delta +1 (pushes the sum). The trailing tombstones
        // (LoadLocal + IntAdd) have deltas +1 and -1 — they cancel
        // when walked as live, mirroring slice 1's shape.
        Op::LoadLocalAddLocal { .. } => 1,
        // Slice-4 fused ops: identical shape to slice 3, just with
        // IntSub / IntMul as terminator. Net delta +1; trailing
        // LoadLocal + IntSub|IntMul tombstones cancel when walked.
        Op::LoadLocalSubLocal { .. } | Op::LoadLocalMulLocal { .. } => 1,
        // Slice-5 fused ops: 4-slot window with net stack delta 0
        // (original sequence had +1, +1, -1, -1). Worklist override
        // above pushes both fall-through and branch successors with
        // this depth; tombstones are not walked as live.
        Op::LoadLocalEqIntConstJumpIfNot { .. }
        | Op::LoadLocalStoreEqIntConstJumpIfNot { .. } => 0,
        // Slice-7 fused op (#461): net +1, same as bare LoadLocal.
        // Trailing GetField (delta 0) + IntAdd (delta -1) tombstones
        // cancel to -1 when walked, leaving depth at pc+3 matching
        // the unfused [LoadLocal, GetField, IntAdd] sequence's
        // overall delta of 0.
        Op::LoadLocalGetFieldAdd { .. } => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;
    use crate::program::Function;

    fn make_fn(name: &str, code: Vec<Op>) -> Function {
        Function {
            name: name.to_string(),
            arity: 0,
            locals_count: 4,
            code,
            effects: vec![],
            body_hash: crate::program::ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        }
    }

    #[test]
    fn clean_match_no_errors() {
        // Simulates a two-arm match that is properly balanced:
        //   LoadLocal(0)               ; push scrutinee   depth=1
        //   Dup                        ; dup              depth=2
        //   TestVariant("Ok")          ; pop+push Bool    depth=2
        //   JumpIfNot(+3)              ; pop Bool, fall or jump  depth=1
        //   Pop                        ; pop scrutinee    depth=0
        //   PushConst(0)               ; push result      depth=1
        //   Jump(+2)                   ; to end           depth=1
        //   Pop                        ; pop scrutinee (wildcard arm) depth=0
        //   PushConst(1)               ; push result      depth=1
        //   Return                     ; end              depth=0
        let code = vec![
            Op::LoadLocal(0),           // pc 0, depth 0→1
            Op::Dup,                    // pc 1, depth 1→2
            Op::TestVariant(0),         // pc 2, depth 2→2
            Op::JumpIfNot(3),           // pc 3, depth 2→1; target=pc7
            Op::Pop,                    // pc 4, depth 1→0
            Op::PushConst(0),           // pc 5, depth 0→1
            Op::Jump(2),                // pc 6, depth 1→1; target=pc9
            Op::Pop,                    // pc 7, depth 1→0  (wildcard arm)
            Op::PushConst(1),           // pc 8, depth 0→1
            Op::Return,                 // pc 9, depth 1→0
        ];
        let f = make_fn("clean", code);
        let mut errs = Vec::new();
        verify_function(&f, &mut errs);
        assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
    }

    #[test]
    fn leaked_scrutinee_detected() {
        // Two paths reach pc6 at different depths — mismatch detected.
        // Fall path: pc2→pc3→pc6 at depth 1.
        // Jump path: pc4→pc5→pc6 at depth 2 (extra push leaks).
        let mismatch2 = vec![
            Op::PushConst(0),    // pc0 depth 0→1
            Op::JumpIfNot(2),    // pc1 depth 1→0; fall=pc2 depth0, jump=pc4 depth0
            Op::PushConst(0),    // pc2 depth 0→1
            Op::Jump(2),         // pc3 target=pc6, depth=1
            Op::PushConst(0),    // pc4 depth 0→1
            Op::PushConst(0),    // pc5 depth 1→2
            Op::Return,          // pc6: reached at depth=1 (from pc3) AND depth=2 (from pc5+fall)
        ];
        let f2 = make_fn("mismatch", mismatch2);
        let mut errs2 = Vec::new();
        verify_function(&f2, &mut errs2);
        assert!(!errs2.is_empty(), "expected stack mismatch error");
        assert_eq!(errs2[0].fn_name, "mismatch");
    }
}
