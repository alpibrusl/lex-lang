//! Peephole passes (#461 and follow-ups): fuse opcode patterns into
//! superinstructions. Every fused op keeps the slots it absorbs in
//! place as inert tombstones, so `code.len()` and all jump offsets are
//! unchanged; `collect_jump_targets` guards against fusing across a
//! jump target. The passes run in a fixed sequence from
//! `compile_program` (see `super`), which also documents the ordering
//! constraints between slices.

use crate::op::*;

/// Peephole pass: rewrite fusable opcode patterns into superinstructions
/// (#461). Each fused op claims its own slot in the code stream; the
/// trailing primitive ops it absorbs stay in place as inert
/// "tombstones" — the dispatch loop overrides its default `pc += 1`
/// to step past them. Leaving the tombstones in place keeps
/// `code.len()` invariant and means we don't have to renumber jump
/// offsets.
///
/// Pattern (slice 1): `LoadLocal(i), PushConst(c), IntAdd` where
/// `constants[c]` is a `Const::Int`. Fused to
/// `LoadLocalAddIntConst { local_idx: i, imm_const_idx: c }`.
/// Safety: the second and third slots must not be reachable from
/// any Jump / JumpIf / JumpIfNot — otherwise a jump would land on a
/// tombstone instead of the live op the source intended. The
/// pre-pass below collects every jump target in the function and
/// skips fusion sites whose tombstones overlap one.
pub(super) fn apply_peephole(code: &mut [Op], constants: &[Const]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(local_idx), Op::PushConst(imm_const_idx), Op::IntAdd)
            = (code[k], code[k + 1], code[k + 2])
        {
            let imm_is_int = matches!(
                constants.get(imm_const_idx as usize),
                Some(Const::Int(_))
            );
            // Tombstones at k+1 and k+2 must not be jump targets;
            // k itself can be a target (it stays a live op — the
            // fused form executes the same semantics in one step).
            let safe = imm_is_int
                && !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2));
            if safe {
                code[k] = Op::LoadLocalAddIntConst { local_idx, imm_const_idx };
                k += 3;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 2: fuse `[LoadLocalAddIntConst, _, _, StoreLocal(dest)]`
/// into `LoadLocalAddIntConstStoreLocal { src, imm_const_idx, dest }`.
/// The two `_` slots are slice-1 tombstones (the original PushConst
/// and IntAdd) and stay in place as slice-2 tombstones too. The
/// dispatch loop advances pc by 4 past all three trailing slots
/// after executing the fused op.
pub(super) fn apply_peephole_slice2(code: &mut [Op]) {
    if code.len() < 4 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 3 < n {
        if let (
            Op::LoadLocalAddIntConst { local_idx: src, imm_const_idx },
            _,
            _,
            Op::StoreLocal(dest),
        ) = (code[k], code[k + 1], code[k + 2], code[k + 3])
        {
            // Slice-1 contract: code[k+1] is the original
            // PushConst(imm_const_idx) and code[k+2] is the
            // original IntAdd. We don't re-verify those — slice 1
            // is the only producer of LoadLocalAddIntConst and
            // always leaves the contract intact.
            //
            // Safety: tombstones at k+1..k+3 must not be reachable
            // from any jump. k itself can be (it's still a live
            // op carrying the same semantics).
            let safe = !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2))
                && !jump_targets.contains(&(k + 3));
            if safe {
                code[k] = Op::LoadLocalAddIntConstStoreLocal {
                    src,
                    imm_const_idx,
                    dest,
                };
                k += 4;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 3: fuse `[LoadLocal(lhs), LoadLocal(rhs), IntAdd]` into
/// `LoadLocalAddLocal { lhs_idx, rhs_idx }`. The binary-op-on-two-
/// locals idiom: any `a + b` where both operands compile to a
/// `LoadLocal` (typed `Int`). Mirrors slice 1's shape exactly — the
/// trailing `LoadLocal` + `IntAdd` stay in place as inert tombstones
/// with cancelling stack deltas (+1, -1), so the verifier and
/// body-hash decoder both keep walking them as live.
///
/// Disjoint from slice 1: the second slot disambiguates (LoadLocal
/// vs PushConst), so a site can match at most one of the two. Runs
/// after slice 2 so we don't accidentally consume a `LoadLocal` slot
/// that slice 2 was about to fuse into a `*StoreLocal` superop (and
/// to keep slice 2's input contract — slice-1 output followed by
/// StoreLocal — untouched).
///
/// Safety: like slice 1, the trailing two slots must not be jump
/// targets. The first slot can be a target (it stays a live op).
pub(super) fn apply_peephole_slice3(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(lhs_idx), Op::LoadLocal(rhs_idx), Op::IntAdd)
            = (code[k], code[k + 1], code[k + 2])
        {
            let safe = !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2));
            if safe {
                code[k] = Op::LoadLocalAddLocal { lhs_idx, rhs_idx };
                k += 3;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 4: slice 3 for `IntSub` and `IntMul`. Fuses
/// `[LoadLocal(lhs), LoadLocal(rhs), IntSub]` to
/// `LoadLocalSubLocal { lhs_idx, rhs_idx }` and the `IntMul` shape
/// to `LoadLocalMulLocal`. Same tombstone, jump-safety, and
/// body-hash story as slice 3 — the trailing two slots stay as
/// inert primitives with cancelling stack deltas.
///
/// Disjoint from every prior slice: slice 1/2 require a `PushConst`
/// at slot 2 (here it's `LoadLocal`), and slice 3's terminator is
/// `IntAdd` (here it's `IntSub` / `IntMul`). A given site matches at
/// most one slice.
pub(super) fn apply_peephole_slice4(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(lhs_idx), Op::LoadLocal(rhs_idx), terminator)
            = (code[k], code[k + 1], code[k + 2])
        {
            let fused = match terminator {
                Op::IntSub => Some(Op::LoadLocalSubLocal { lhs_idx, rhs_idx }),
                Op::IntMul => Some(Op::LoadLocalMulLocal { lhs_idx, rhs_idx }),
                _ => None,
            };
            if let Some(fused_op) = fused {
                let safe = !jump_targets.contains(&(k + 1))
                    && !jump_targets.contains(&(k + 2));
                if safe {
                    code[k] = fused_op;
                    k += 3;
                    continue;
                }
            }
        }
        k += 1;
    }
}

/// Slice 5: fuse the loop-condition idiom — 4-slot window
/// `[LoadLocal, LoadLocal|PushConst, IntLt, JumpIfNot(offset)]` —
/// into `LoadLocalLtLocalJumpIfNot` or `LoadLocalLtIntConstJumpIfNot`.
/// First jump-aware peephole in this codebase: the fused op carries
/// the JumpIfNot's offset and the VM dispatches directly to either
/// `pc + 4` (condition true, fall through past tombstones) or
/// `pc + 4 + offset` (condition false, original JumpIfNot target).
///
/// Safety conditions, on top of slice 1's "tombstones must not be
/// jump targets":
/// 1. Trailing 3 slots (k+1, k+2, k+3) must not be jump targets from
///    elsewhere — same as slice 1/3/4, just three of them.
/// 2. The slot at k+3 (JumpIfNot) is the one whose offset we copy
///    into the fused op. The offset is relative to the JumpIfNot's
///    `pc + 1` which equals `k + 4`, so the resolved target is
///    `k + 4 + offset` — that target must be safe to land on (it
///    already is, since JumpIfNot is operating as designed).
/// 3. The const-int branch checks the PushConst points at a
///    `Const::Int` — same as slice 1.
pub(super) fn apply_peephole_slice5(code: &mut [Op], constants: &[Const]) {
    if code.len() < 4 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 3 < n {
        // Match the lhs slot — always a LoadLocal.
        let lhs_idx = match code[k] {
            Op::LoadLocal(i) => i,
            _ => { k += 1; continue; }
        };
        // Match the rhs slot — either LoadLocal or PushConst(Int).
        // The two flavors emit different fused ops.
        let fused = match (code[k + 1], code[k + 2], code[k + 3]) {
            (Op::PushConst(imm_const_idx), Op::IntEq, Op::JumpIfNot(jump_offset))
                if matches!(constants.get(imm_const_idx as usize), Some(Const::Int(_))) =>
                Some(Op::LoadLocalEqIntConstJumpIfNot {
                    local_idx: lhs_idx, imm_const_idx, jump_offset,
                }),
            _ => None,
        };
        if let Some(fused_op) = fused {
            let safe = !jump_targets.contains(&(k + 1))
                && !jump_targets.contains(&(k + 2))
                && !jump_targets.contains(&(k + 3));
            if safe {
                code[k] = fused_op;
                k += 4;
                continue;
            }
        }
        k += 1;
    }
}

/// Slice 6: fuse the match-scrutinee dance preceding a slice-5
/// pattern-match arm test. 3-slot window
/// `[LoadLocal(src), StoreLocal(dst),
///   LoadLocalEqIntConstJumpIfNot { local_idx: dst, ... }]` —
/// where the slice-5 op's `local_idx` matches the StoreLocal's
/// destination — rewrites to
/// `LoadLocalStoreEqIntConstJumpIfNot { src, dst, ... }` at slot k.
/// The fused op carries `dst` so it can mirror the original
/// StoreLocal (later arm tests in the same match keep reading
/// `locals[dst]`).
///
/// Trailing tombstones: 5 slots (the original StoreLocal + the
/// slice-5 fused op itself + slice 5's 3 primitive tombstones).
/// VM dispatch skips them via `pc + 6`; verifier override pushes
/// `(pc + 6, ...)` and the branch target `(pc + 6 + jump_offset, ...)`
/// — the offset is identical to what slice 5 stored (still relative
/// to the original JumpIfNot's `pc + 1`, now at `k + 5 + 1 = k + 6`).
///
/// Safety: slots k+1..=k+5 must not be jump targets — same window
/// safety as the other slices. Slice 5 already verified k+3..=k+5
/// weren't jump targets when it fused; slice 6 only needs to re-check
/// k+1 (the StoreLocal) and k+2 (the slice-5 fused op).
pub(super) fn apply_peephole_slice6(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (
            Op::LoadLocal(src),
            Op::StoreLocal(dst),
            Op::LoadLocalEqIntConstJumpIfNot { local_idx, imm_const_idx, jump_offset },
        ) = (code[k], code[k + 1], code[k + 2]) {
            // The slice-5 op must read the very local the StoreLocal
            // just wrote; if it reads some other local this isn't the
            // match-scrutinee idiom (could be a coincidental sequence).
            if local_idx == dst {
                let safe = !jump_targets.contains(&(k + 1))
                    && !jump_targets.contains(&(k + 2));
                if safe {
                    code[k] = Op::LoadLocalStoreEqIntConstJumpIfNot {
                        src, dst, imm_const_idx, jump_offset,
                    };
                    // Skip past this slice-6 window. The slice-5
                    // tombstones at k+3..=k+5 are already handled by
                    // slice 5's earlier rewrite; we don't need to
                    // touch them.
                    k += 3;
                    continue;
                }
            }
        }
        k += 1;
    }
}

/// Slice 7/8: fuse `[LoadLocal(local_idx), GetField{name_idx,
/// site_idx}, IntAdd|IntSub|IntMul]` into the matching
/// `LoadLocalGetField{Add,Sub,Mul} { local_idx, name_idx, site_idx }`.
///
/// Fires on the `acc OP r.field` accumulator-with-field-read idiom —
/// the bytecode the compiler emits for `prev_expr OP record.field`
/// once `prev_expr` is on the stack. Common in handler-shaped code
/// like `r.x + r.y + r.z` (the LHS of each operator after the first
/// matches this pattern), `acc + items[i].weight` reductions, and
/// the `v.l - v.m` / `v.h * v.k` mixes the `response_build` profile
/// exercises.
///
/// Disjoint from every prior slice: slice 1 wants `PushConst` at
/// slot 1; slices 3-4 want `LoadLocal` at slot 1; slice 5 wants
/// a 4-slot window with `IntEq + JumpIfNot` terminator. Only this
/// slice matches a `GetField` at slot 1.
///
/// Order: must run after slice 4 (so the disjointness analysis
/// holds — slice 3/4 patterns with a trailing IntAdd / IntSub /
/// IntMul never carry a GetField at slot 1 and don't compete);
/// must run before / independent of slice 5/6, which don't match
/// any slot in this window.
///
/// Safety: trailing two slots (the original `GetField` and the
/// arithmetic op) must not be jump targets. The first slot can be.
pub(super) fn apply_peephole_slice7(code: &mut [Op]) {
    if code.len() < 3 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 2 < n {
        if let (Op::LoadLocal(local_idx), Op::GetField { name_idx, site_idx })
            = (code[k], code[k + 1])
        {
            let fused = match code[k + 2] {
                Op::IntAdd => Some(Op::LoadLocalGetFieldAdd { local_idx, name_idx, site_idx }),
                Op::IntSub => Some(Op::LoadLocalGetFieldSub { local_idx, name_idx, site_idx }),
                Op::IntMul => Some(Op::LoadLocalGetFieldMul { local_idx, name_idx, site_idx }),
                _ => None,
            };
            if let Some(op) = fused {
                let safe = !jump_targets.contains(&(k + 1))
                    && !jump_targets.contains(&(k + 2));
                if safe {
                    code[k] = op;
                    k += 3;
                    continue;
                }
            }
        }
        k += 1;
    }
}

/// Slice 9: fuse the bare `[LoadLocal(local_idx), GetField{name_idx,
/// site_idx}]` pair into `LoadLocalGetField { local_idx, name_idx,
/// site_idx }` — the plain `record.field` read, the most common
/// field-access shape.
///
/// The win is allocation, not just one fewer dispatch: the unfused
/// pair clones the entire record onto the value stack (a
/// `Box<IndexMap>` for a heap record) only to read one field; the
/// fused op reads the field out of the local by reference and clones
/// only that value. On `response_build` the whole-record clone of the
/// returned `Response` (`r.total`) was the dominant malloc source.
///
/// Order: MUST run after slice 7/8. Those fuse `[LoadLocal, GetField,
/// IntAdd|IntSub|IntMul]`; if slice 9 ran first it would consume the
/// `LoadLocal + GetField` prefix and block the 3-op fusion. After
/// slice 7/8, the only remaining `[LoadLocal, GetField]` pairs are
/// the ones they didn't want (chain heads, standalone reads, field
/// reads feeding other ops). Slice 7/8's tombstone GetFields sit
/// after their fused op, never after a bare `LoadLocal`, so slice 9
/// won't touch them.
///
/// Safety: the trailing slot (the original `GetField`) must not be a
/// jump target. The first slot can be.
pub(super) fn apply_peephole_slice9(code: &mut [Op]) {
    if code.len() < 2 { return; }
    let jump_targets = collect_jump_targets(code);

    let n = code.len();
    let mut k = 0;
    while k + 1 < n {
        if let (Op::LoadLocal(local_idx), Op::GetField { name_idx, site_idx })
            = (code[k], code[k + 1])
        {
            if !jump_targets.contains(&(k + 1)) {
                code[k] = Op::LoadLocalGetField { local_idx, name_idx, site_idx };
                k += 2;
                continue;
            }
        }
        k += 1;
    }
}

fn collect_jump_targets(code: &[Op]) -> std::collections::HashSet<usize> {
    let mut targets = std::collections::HashSet::new();
    for (pc, op) in code.iter().enumerate() {
        let off = match op {
            Op::Jump(off) | Op::JumpIf(off) | Op::JumpIfNot(off) => Some(*off),
            _ => None,
        };
        if let Some(off) = off {
            let target = (pc as i32 + 1 + off) as usize;
            targets.insert(target);
        }
    }
    targets
}
