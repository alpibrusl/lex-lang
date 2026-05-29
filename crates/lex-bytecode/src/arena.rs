//! Request-scope arena-eligibility analysis (#463 slice 1).
//!
//! Per-allocation-site classification: is this `MakeRecord` /
//! `MakeTuple` value safe to route through the active per-request
//! arena (`EffectHandler::enter_request_scope`), instead of the
//! global allocator?
//!
//! ## Relationship to `escape::analyze_function` (#464)
//!
//! Same lattice, same worklist, same step rules — with **one bit of
//! policy flipped**: `Op::Return` is not an escape op here, because
//! the returned value goes to the caller's stack and the caller is
//! in the same request scope as us. Everything else (`Call`,
//! `CallClosure`, `TailCall`, `EffectCall`, `MakeClosure` captures,
//! aggregate-as-field, worker-pool ops, `Dup`, …) stays a hatch under
//! the slice-1 intra-procedural conservative policy. The shared
//! machinery is `escape::analyze_function_with_policy(_,
//! Policy::RequestScope)`; this module wraps it and inverts the
//! per-site `escapes` bool into `arena_eligible`.
//!
//! ## Slice scope
//!
//! Analysis only. No opcode lowering, no runtime behavior change, no
//! bytecode-format change. Slice 2 (`AllocArenaRecord` /
//! `AllocArenaList` / handle variants on `Value`) consults
//! `build_arena_index` at codegen time.
//!
//! ## Soundness contract
//!
//! Inherits #464's contract verbatim (`docs/design/escape-analysis.md`
//! § "Soundness contract"):
//!
//! - **Over-approximation** (`arena_eligible = false` when the value
//!   actually stays in-scope) costs a heap allocation — the
//!   status-quo baseline. Acceptable.
//! - **Under-approximation** (`arena_eligible = true` when the value
//!   actually escapes the request) would let an arena handle outlive
//!   its slab and is UB. Slice 2 must pair this analysis with an
//!   unconditional runtime fallback (same shape as #464's
//!   `AllocStackRecord` heap fallback), so a missed hatch costs
//!   correctness only if the analysis is wrong *and* the fallback is
//!   omitted — never both.
//!
//! ## Out of scope for slice 1
//!
//! - Inter-procedural escape (the scoping doc defers this until
//!   inlining lands with #465 phase 1; any `Call` is a hatch here).
//! - Worker-handler lifetime split (`spawn_for_worker` clone-handlers
//!   get a fresh empty arena stack — values handed to workers must
//!   never become arena handles). Already covered by the conservative
//!   hatches on `Call`/`ParallelMap`/`SortByKey`; slice 2 must keep
//!   that invariant when routing.

use std::collections::HashMap;

use crate::escape::{analyze_function_with_policy, Policy, SiteKind};
use crate::program::Function;

/// Per-function arena-eligibility report. Mirrors `EscapeReport`'s
/// shape with the per-site bool inverted (`arena_eligible =
/// !escapes_under_request_scope`) and renamed to reflect what
/// downstream codegen will use it for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArenaReport {
    pub fn_name: String,
    pub sites: Vec<ArenaSite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ArenaSite {
    pub pc: u32,
    pub kind: SiteKind,
    pub shape_idx: u32,
    pub field_count: u16,
    /// `true` if the value never leaves the request scope on any
    /// reachable path — safe to allocate from the active request
    /// arena. `false` means a hatch (`Call`, `EffectCall`,
    /// `MakeClosure` capture, worker-pool op, …) is reachable — keep
    /// on the heap.
    pub arena_eligible: bool,
}

/// Analyze one function. Cheap on functions with no aggregate sites
/// (early-exits in the underlying pass).
pub fn analyze_function(func: &Function) -> ArenaReport {
    let r = analyze_function_with_policy(func, Policy::RequestScope);
    let sites = r
        .sites
        .into_iter()
        .map(|s| ArenaSite {
            pc: s.pc,
            kind: s.kind,
            shape_idx: s.shape_idx,
            field_count: s.field_count,
            arena_eligible: !s.escapes,
        })
        .collect();
    ArenaReport { fn_name: r.fn_name, sites }
}

/// Analyze every function. Functions with no aggregate sites are
/// omitted from the result, matching `escape::analyze_program`.
pub fn analyze_program(functions: &[Function]) -> Vec<ArenaReport> {
    functions
        .iter()
        .filter_map(|f| {
            let r = analyze_function(f);
            (!r.sites.is_empty()).then_some(r)
        })
        .collect()
}

/// Convenience map keyed by `(fn_name, pc)` for direct lookup during
/// the slice-2 codegen pass. Mirrors `escape::build_escape_index`
/// exactly so the codegen swap is structural.
pub fn build_arena_index(functions: &[Function]) -> HashMap<(String, u32), bool> {
    let mut idx = HashMap::new();
    for report in analyze_program(functions) {
        for site in report.sites {
            idx.insert((report.fn_name.clone(), site.pc), site.arena_eligible);
        }
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;
    use crate::program::{Function, ZERO_BODY_HASH};

    fn func(name: &str, locals_count: u16, arity: u16, code: Vec<Op>) -> Function {
        Function {
            name: name.into(),
            arity,
            locals_count,
            code,
            effects: vec![],
            body_hash: ZERO_BODY_HASH,
            refinements: vec![],
            field_ic_sites: 0,
        }
    }

    fn assert_eligible(report: &ArenaReport, expected: &[(u32, bool)]) {
        let got: Vec<(u32, bool)> = report
            .sites
            .iter()
            .map(|s| (s.pc, s.arena_eligible))
            .collect();
        assert_eq!(
            got, expected,
            "arena eligibility for `{}` differs from expected",
            report.fn_name
        );
    }

    // ---- The slice's reason for existing: Return is not a hatch ----

    /// A record built and returned (e.g. the `Response` the handler
    /// hands up to `net.serve_fn`) is arena-eligible. Under #464's
    /// frame-scope policy this identical shape escapes — the
    /// divergence is the whole point.
    #[test]
    fn record_returned_is_arena_eligible() {
        let f = func("handler", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(2, true)]);
    }

    /// Tuple returned: same divergence as record. Confirms the
    /// policy bit applies uniformly across aggregate kinds.
    #[test]
    fn tuple_returned_is_arena_eligible() {
        let f = func("handler_t", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(2, true)]);
    }

    /// Round-trip through a local, then returned. The local read
    /// keeps the slot tracked, and the Return doesn't escape under
    /// request scope. End-to-end arena-eligible.
    #[test]
    fn record_round_tripped_and_returned_is_arena_eligible() {
        let f = func("handler_rt", 1, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::StoreLocal(0),
            Op::LoadLocal(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(2, true)]);
    }

    // ---- All other hatches still apply (parity with #464) ----

    /// Slice 1's intra-procedural conservatism: any `Call` into a
    /// non-inlined helper is a hatch. Args may leak via the callee's
    /// own escape paths (`spawn`, channel send, module-level store).
    #[test]
    fn record_passed_to_call_is_not_arena_eligible() {
        let f = func("caller", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(1, false)]);
    }

    /// Closure capture is a hatch — closures may outlive the request
    /// (stored in module-level state, returned to the runtime, …).
    #[test]
    fn record_captured_in_closure_is_not_arena_eligible() {
        let f = func("capturer", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::MakeClosure { fn_id: 1, capture_count: 1 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(1, false)]);
    }

    /// EffectCall is a hatch — effect handlers can spawn workers,
    /// send on channels, persist to disk: any path that outlives the
    /// request.
    #[test]
    fn record_passed_to_effect_is_not_arena_eligible() {
        let f = func("effecting", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::EffectCall { kind_idx: 0, op_idx: 0, arity: 1, node_id_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(1, false)]);
    }

    // ---- Site bookkeeping ----

    #[test]
    fn record_dropped_is_arena_eligible() {
        let f = func("drop", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(1, true)]);
    }

    /// Inner record stored as a field of outer; outer returned.
    /// Inner escapes (consumed by the outer aggregate's heap
    /// constructor — slice-1 doesn't yet model "outer is also
    /// arena → inner can live with it"; that's a slice-2 codegen
    /// question). Outer is arena-eligible — its only consumer is
    /// `Return`, which doesn't escape under request scope.
    #[test]
    fn outer_returned_aggregate_is_arena_eligible_inner_field_is_not() {
        let f = func("nest", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // inner @ pc=1
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 1, field_count: 2 }, // outer @ pc=3
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(1, false), (3, true)]);
    }

    /// Two sites in one function, independently classified: one
    /// returned (arena), one passed to a call (heap).
    #[test]
    fn two_sites_classified_independently() {
        let f = func("mixed", 1, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // pc=1: kept, returned
            Op::StoreLocal(0),
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // pc=4: passed to call
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 0 },
            Op::Pop,
            Op::LoadLocal(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eligible(&r, &[(1, true), (4, false)]);
    }

    #[test]
    fn build_arena_index_keys_by_fn_and_pc() {
        let f = func("idx_test", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Return,
        ]);
        let idx = build_arena_index(&[f]);
        assert_eq!(idx.get(&("idx_test".into(), 1)), Some(&true));
    }

    #[test]
    fn analyze_program_skips_functions_with_no_sites() {
        let f1 = func("noaggs", 0, 0, vec![Op::PushConst(0), Op::Return]);
        let f2 = func("hasagg", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Return,
        ]);
        let reports = analyze_program(&[f1, f2]);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].fn_name, "hasagg");
    }

    // ---- Parity sanity vs #464 ----

    /// Where the two analyses *should* agree (a Call hatch is a hatch
    /// regardless of policy), they do.
    #[test]
    fn parity_with_frame_escape_on_shared_hatch() {
        use crate::escape::analyze_function as analyze_frame;
        let f = func("parity_hatch", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 0 },
            Op::Return,
        ]);
        let arena = analyze_function(&f);
        let frame = analyze_frame(&f);
        assert!(!arena.sites[0].arena_eligible);
        assert!(frame.sites[0].escapes);
    }

    /// Where the two *should* diverge (a plain Return), they do —
    /// this test is the documented intentional difference and would
    /// fire if the policy bit ever got dropped in a refactor.
    #[test]
    fn diverges_from_frame_escape_on_return() {
        use crate::escape::analyze_function as analyze_frame;
        let f = func("parity_return", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Return,
        ]);
        let arena = analyze_function(&f);
        let frame = analyze_frame(&f);
        assert!(arena.sites[0].arena_eligible);
        assert!(frame.sites[0].escapes);
    }
}
