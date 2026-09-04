//! Allocation-lowering passes (#464 step 2, #463 slice 2b-i): rewrite
//! `MakeRecord` / `MakeTuple` sites the escape and arena analyses
//! proved frame- or request-local into their stack / arena variants.
//! Each rewrite is a single-slot swap, so pc, stack delta and shape
//! semantics are preserved and `compute_body_hash` decodes both
//! forms back to the legacy op (closure identity is invariant).
//! Called from `compile_program` in a fixed order; see `super`.

use crate::op::*;

/// #464 step 2 — rewrite `MakeRecord` to `AllocStackRecord` at sites
/// the escape analysis (`crate::escape::build_escape_index`) proved
/// non-escaping. Each rewrite is a single-slot swap that preserves
/// pc, stack delta, and shape semantics — jump targets, the peephole
/// passes downstream, and the body-hash decoder all see the same
/// program shape they would have seen for the unlowered code.
///
/// Sites that escape are left as-is and still incur the
/// IndexMap-backed heap allocation. Step 3 of #464 carries the
/// bench acceptance bars (≥1.5× speedup on `response_build`); this
/// pass is the precondition.
pub(super) fn apply_escape_lowering(
    code: &mut [Op],
    fn_name: &str,
    escape_index: &std::collections::HashMap<(String, u32), bool>,
) {
    for (pc, op) in code.iter_mut().enumerate() {
        // Look up this (fn, pc) in the escape index. Absent → analysis
        // didn't observe the site (defensive: leave on heap path).
        // Present and false → safe to stack-allocate. Each rewrite is a
        // single-slot swap preserving pc / stack delta, so jump
        // targets, downstream peephole passes, and the body-hash
        // decoder all see the same program shape.
        let key = (fn_name.to_string(), pc as u32);
        if !matches!(escape_index.get(&key), Some(false)) {
            continue;
        }
        match *op {
            Op::MakeRecord { shape_idx, field_count } => {
                *op = Op::AllocStackRecord { shape_idx, field_count };
            }
            // #464 tuple codegen: same single-slot swap as records.
            Op::MakeTuple(arity) => {
                *op = Op::AllocStackTuple { arity };
            }
            _ => {}
        }
    }
}

/// #463 slice 2b-i — rewrite `MakeRecord` / `MakeTuple` to the arena
/// variants at sites the request-scope analysis
/// (`crate::arena::build_arena_index`) proved do not escape the
/// active `EffectHandler` arena scope.
///
/// Only fires on **remaining** `MakeRecord` / `MakeTuple` sites — the
/// stack pass (`apply_escape_lowering`) runs first and converts the
/// non-frame-escaping cheaper-tier sites. Sites that escape both the
/// frame *and* the request stay as `MakeRecord` / `MakeTuple` (heap),
/// untouched.
///
/// Each rewrite is the same single-slot swap as the stack lowering:
/// pc / stack delta / shape semantics preserved, jump targets and
/// downstream peephole passes see the same program shape, and
/// `compute_body_hash` (#222) decodes both arena ops back to their
/// legacy `MakeRecord` / `MakeTuple` form so closure identity is
/// invariant.
pub(super) fn apply_arena_lowering(
    code: &mut [Op],
    fn_name: &str,
    arena_index: &std::collections::HashMap<(String, u32), bool>,
) {
    for (pc, op) in code.iter_mut().enumerate() {
        // arena_index value: true = arena-eligible. Absent or false
        // → leave on heap (defensive default; absent means the
        // analysis didn't observe the site).
        let key = (fn_name.to_string(), pc as u32);
        if !matches!(arena_index.get(&key), Some(true)) {
            continue;
        }
        match *op {
            Op::MakeRecord { shape_idx, field_count } => {
                *op = Op::AllocArenaRecord { shape_idx, field_count };
            }
            Op::MakeTuple(arity) => {
                *op = Op::AllocArenaTuple { arity };
            }
            _ => {}
        }
    }
}
