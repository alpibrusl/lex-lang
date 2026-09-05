//! Last-use analysis (#774): backward liveness over a function's
//! bytecode CFG, used to rewrite the final `LoadLocal` of a slot into
//! a moving `TakeLocal`. Runs after the peepholes in `compile_program`
//! (see `super`); the `LEX_NO_TAKE_LOCALS=1` escape hatch there skips
//! it.

use crate::op::*;

/// #774: rewrite a `LoadLocal` whose slot is dead afterwards to
/// `TakeLocal`, so the value moves out instead of being cloned.
///
/// Backward liveness over the function's control flow: an
/// instruction's live-out set is the union of its successors'
/// live-in sets, and live-in is live-out minus the slots it writes
/// plus the slots it reads. A plain load whose slot is not live-out
/// is the last read on every path through it, which makes the move
/// sound — including in the loops the inlined `std.iter` lowering
/// emits (the back edge keeps a slot live across iterations) and in
/// sibling `match` arms (each arm's load is judged on its own path,
/// so an accumulator read in two arms moves in both).
///
/// Runs after the peepholes. Fused superinstructions read their
/// slots by index and are never rewritten; the tombstone ops they
/// leave behind are treated as if executed, which is a conservative
/// over-approximation of reads and produces the same control-flow
/// edges as the fused op itself.
pub(super) fn apply_last_load_takes(code: &mut [Op]) {
    let n = code.len();
    if n == 0 {
        return;
    }
    // Slot reads / writes / successors per instruction.
    fn reads(op: &Op) -> Vec<u16> {
        match op {
            Op::LoadLocal(i) | Op::TakeLocal(i) => vec![*i],
            Op::LoadLocalAddIntConst { local_idx, .. }
            | Op::LoadLocalEqIntConstJumpIfNot { local_idx, .. }
            | Op::LoadLocalGetFieldAdd { local_idx, .. }
            | Op::LoadLocalGetFieldSub { local_idx, .. }
            | Op::LoadLocalGetFieldMul { local_idx, .. }
            | Op::LoadLocalGetField { local_idx, .. } => vec![*local_idx],
            Op::LoadLocalAddIntConstStoreLocal { src, .. }
            | Op::LoadLocalStoreEqIntConstJumpIfNot { src, .. } => vec![*src],
            Op::LoadLocalAddLocal { lhs_idx, rhs_idx }
            | Op::LoadLocalSubLocal { lhs_idx, rhs_idx }
            | Op::LoadLocalMulLocal { lhs_idx, rhs_idx } => vec![*lhs_idx, *rhs_idx],
            _ => Vec::new(),
        }
    }
    fn writes(op: &Op) -> Option<u16> {
        match op {
            Op::StoreLocal(i) => Some(*i),
            Op::LoadLocalAddIntConstStoreLocal { dest, .. } => Some(*dest),
            Op::LoadLocalStoreEqIntConstJumpIfNot { dst, .. } => Some(*dst),
            _ => None,
        }
    }
    fn succs(pc: usize, op: &Op, n: usize) -> Vec<usize> {
        let at = |base: i64, off: i32| -> Option<usize> {
            let t = base + off as i64;
            if t >= 0 && (t as usize) < n { Some(t as usize) } else { None }
        };
        let next = if pc + 1 < n { Some(pc + 1) } else { None };
        let v: Vec<Option<usize>> = match op {
            Op::Jump(off) => vec![at(pc as i64 + 1, *off)],
            Op::JumpIf(off) | Op::JumpIfNot(off) => vec![next, at(pc as i64 + 1, *off)],
            // Fused jumps (#461 slice 5/6): target offsets are relative
            // to the end of the fused window, as in `verify.rs`.
            Op::LoadLocalEqIntConstJumpIfNot { jump_offset, .. } => vec![next, at(pc as i64 + 4, *jump_offset)],
            Op::LoadLocalStoreEqIntConstJumpIfNot { jump_offset, .. } => vec![next, at(pc as i64 + 6, *jump_offset)],
            Op::Return | Op::Panic(_) | Op::TailCall { .. } => vec![],
            _ => vec![next],
        };
        v.into_iter().flatten().collect()
    }

    let nslots = code
        .iter()
        .flat_map(|op| reads(op).into_iter().chain(writes(op)))
        .map(|s| s as usize + 1)
        .max()
        .unwrap_or(0);
    if nslots == 0 {
        return;
    }
    let succ: Vec<Vec<usize>> = code.iter().enumerate().map(|(pc, op)| succs(pc, op, n)).collect();
    let rd: Vec<Vec<u16>> = code.iter().map(reads).collect();
    let wr: Vec<Option<u16>> = code.iter().map(writes).collect();
    let mut live_in: Vec<Vec<bool>> = vec![vec![false; nslots]; n];
    let mut live_out: Vec<Vec<bool>> = vec![vec![false; nslots]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for pc in (0..n).rev() {
            let mut out = vec![false; nslots];
            for &s in &succ[pc] {
                for (k, v) in live_in[s].iter().enumerate() {
                    if *v { out[k] = true; }
                }
            }
            let mut inn = out.clone();
            if let Some(w) = wr[pc] { inn[w as usize] = false; }
            for &r in &rd[pc] { inn[r as usize] = true; }
            if out != live_out[pc] || inn != live_in[pc] {
                live_out[pc] = out;
                live_in[pc] = inn;
                changed = true;
            }
        }
    }
    // Tombstones: the primitive ops a fused superinstruction replaced
    // stay in the stream (the dispatcher skips them) and hash as
    // themselves; they never execute, so rewriting one is pointless
    // and would disturb the shape the peephole tests pin down.
    let mut tombstone = vec![false; n];
    for (pc, op) in code.iter().enumerate() {
        let width = match op {
            Op::LoadLocalGetField { .. } => 2,
            Op::LoadLocalAddIntConst { .. }
            | Op::LoadLocalAddLocal { .. }
            | Op::LoadLocalSubLocal { .. }
            | Op::LoadLocalMulLocal { .. }
            | Op::LoadLocalGetFieldAdd { .. }
            | Op::LoadLocalGetFieldSub { .. }
            | Op::LoadLocalGetFieldMul { .. } => 3,
            Op::LoadLocalAddIntConstStoreLocal { .. }
            | Op::LoadLocalEqIntConstJumpIfNot { .. } => 4,
            Op::LoadLocalStoreEqIntConstJumpIfNot { .. } => 6,
            _ => 1,
        };
        for slot in tombstone.iter_mut().skip(pc + 1).take(width - 1) {
            *slot = true;
        }
    }
    for (pc, op) in code.iter_mut().enumerate() {
        if tombstone[pc] {
            continue;
        }
        if let Op::LoadLocal(i) = *op {
            if !live_out[pc][i as usize] {
                *op = Op::TakeLocal(i);
            }
        }
    }
}
