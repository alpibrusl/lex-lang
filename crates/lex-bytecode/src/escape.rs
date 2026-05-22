//! Escape analysis for stack-allocatable aggregate sites —
//! `Op::MakeRecord` and `Op::MakeTuple` (#464 + tuple widening).
//!
//! Walks every function's bytecode to classify each aggregate
//! allocation site as either **stack-allocatable** (the value never
//! leaves the function frame) or **escapes** (used as a closure
//! capture, returned, stored in another aggregate, passed to a call,
//! sent on a channel, etc.). The escape rules are identical for
//! records and tuples — only the eventual codegen opcode differs —
//! so a single `Slot::Agg(pc)` lattice value tracks both.
//!
//! ## Status
//!
//! - **Records** (`MakeRecord`): analysis + codegen complete (#464).
//!   `compiler::apply_escape_lowering` rewrites non-escaping
//!   `MakeRecord` to `AllocStackRecord`.
//! - **Tuples** (`MakeTuple`): analysis only, this slice. Sites are
//!   classified and reported with `SiteKind::Tuple`, but no codegen
//!   consumes them yet — `apply_escape_lowering` only rewrites
//!   `MakeRecord`, so reporting tuple sites is inert until a tuple
//!   stack-alloc opcode lands. Mirrors how #464 sequenced records
//!   (analysis → codegen → bench).
//!
//! ## Approach
//!
//! Abstract interpretation over the bytecode CFG. Each abstract
//! state tracks:
//! - per-stack-slot: `Slot::Agg(pc)` (the aggregate allocated at
//!   `pc`, still local) or `Slot::Other` (anything else — int,
//!   string, captured value, aggregate we've stopped tracking)
//! - per-local: same `Slot` lattice, indexed by `locals[i]`
//!
//! At each op we update the abstract state and union any newly-
//! observed escapes into a `HashSet<u32>` keyed by allocation pc.
//! Worklist fixpoint iterates until no state changes — joins use a
//! pointwise merge that downgrades `Agg(a) ⊔ Agg(b)` (a ≠ b) and
//! `Agg(a) ⊔ Other` to `Other`, marking the involved sites as
//! escaped (we can no longer prove they stay local from this
//! merge point forward).
//!
//! ## Intra-procedural limit
//!
//! Calls (`Call`, `TailCall`, `CallClosure`) escape all argument
//! aggregates — we can't see the callee's body from here. Inlining
//! could recover the cross-fn case but is deliberately out of
//! scope; the issue's wording ("function frame") matches
//! intra-procedural.
//!
//! ## Conservative defaults
//!
//! Whenever the analysis can't prove an aggregate stays local, it
//! defaults to *escaped*. False positives (sites flagged as
//! escaping when they actually don't) cost a heap allocation per
//! request — the existing baseline. False negatives (a flagged-
//! local site that actually escapes) would corrupt memory under
//! stack-alloc codegen, so the codegen step treats the analysis
//! output as a *necessary* but not sufficient precondition and
//! pairs it with an unconditional runtime fallback.

use std::collections::{HashMap, HashSet};

use crate::op::Op;
use crate::program::Function;

/// Abstract value at a stack or local slot during the analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// Holds the stack-allocatable aggregate (a record from
    /// `Op::MakeRecord` or a tuple from `Op::MakeTuple`) allocated at
    /// this pc. As long as every consumer reads this slot via a field
    /// / element read (`GetField` / `GetElem`), `Pop`, or a
    /// `StoreLocal`/`LoadLocal` round-trip, the site stays a
    /// stack-alloc candidate. The escape rules are identical for both
    /// aggregate kinds — only the codegen opcode differs — so the
    /// analysis tracks them with one variant keyed on the alloc pc.
    Agg(u32),
    /// Anything else — primitives, already-escaped aggregates,
    /// values produced by ops we don't model precisely. Treated
    /// as not-a-tracked-aggregate for escape purposes.
    Other,
}

impl Slot {
    /// Pointwise merge for join points. Same site survives;
    /// anything else collapses to `Other`. Callers responsible for
    /// recording any `Rec(_)` that was merged-away into the
    /// escape set — we lose track of those sites past this merge.
    fn merge(self, other: Slot) -> Slot {
        match (self, other) {
            (Slot::Agg(a), Slot::Agg(b)) if a == b => Slot::Agg(a),
            _ => Slot::Other,
        }
    }
}

/// Abstract state at one program point: the value stack from
/// bottom up, plus a flat local-variable map.
#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    stack: Vec<Slot>,
    locals: Vec<Slot>,
}

impl State {
    fn entry(locals_count: usize, arity: usize) -> Self {
        // Function parameters land in the first `arity` locals;
        // they're potentially-escaped values handed in by the
        // caller, but the *sites* that produced them live in the
        // caller's frame and aren't our concern. Treat as Other.
        Self {
            stack: Vec::new(),
            locals: vec![Slot::Other; locals_count.max(arity)],
        }
    }

    /// Merge `other` into `self`. Returns `(merged_state, escaped_sites)`
    /// — the sites that we lost track of during the merge. Callers
    /// union the escapes into the function-level set.
    fn merge_with(&self, other: &State) -> (State, HashSet<u32>) {
        let mut escaped = HashSet::new();
        // Mismatched stack depths are a verifier-level bug (#347
        // already checks this); for the escape analysis we just
        // truncate to the shorter and proceed — any sites on the
        // extra slots count as escapes since they're no longer
        // reachable from the join state.
        let stack_len = self.stack.len().min(other.stack.len());
        let mut stack = Vec::with_capacity(stack_len);
        for i in 0..stack_len {
            let merged = self.stack[i].merge(other.stack[i]);
            // If a Rec was merged-away (either path had Rec, the
            // result is Other), the corresponding site escapes.
            if merged == Slot::Other {
                if let Slot::Agg(p) = self.stack[i]  { escaped.insert(p); }
                if let Slot::Agg(p) = other.stack[i] { escaped.insert(p); }
            }
            stack.push(merged);
        }
        // The dropped tail of the longer stack also leaks any Rec.
        for tail in self.stack.iter().skip(stack_len).chain(other.stack.iter().skip(stack_len)) {
            if let Slot::Agg(p) = tail { escaped.insert(*p); }
        }
        let local_len = self.locals.len().max(other.locals.len());
        let mut locals = Vec::with_capacity(local_len);
        for i in 0..local_len {
            let a = self.locals.get(i).copied().unwrap_or(Slot::Other);
            let b = other.locals.get(i).copied().unwrap_or(Slot::Other);
            let merged = a.merge(b);
            if merged == Slot::Other {
                if let Slot::Agg(p) = a { escaped.insert(p); }
                if let Slot::Agg(p) = b { escaped.insert(p); }
            }
            locals.push(merged);
        }
        (State { stack, locals }, escaped)
    }
}

/// Per-function escape report — the artifact codegen consumes to
/// decide where to emit a stack-alloc opcode vs the heap constructor.
///
/// Each entry is keyed by the allocation pc (the `Op::MakeRecord` or
/// `Op::MakeTuple` site's index in the function's `code` vec) and
/// tagged with its `SiteKind`. `escapes = false` means: across every
/// reachable path through the function, the aggregate allocated here
/// is only ever read locally (`GetField` / `GetElem`, dropped via
/// `Pop`, round-tripped through locals) — never returned, captured,
/// stored in another aggregate, or passed to a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapeReport {
    pub fn_name: String,
    /// One entry per stack-allocatable aggregate (`MakeRecord` /
    /// `MakeTuple`) site in the function, in pc order.
    pub sites: Vec<EscapeSite>,
}

/// Which aggregate constructor an escape site is — determines which
/// stack-alloc opcode a future codegen slice would emit for it
/// (`AllocStackRecord` for records; tuple stack-alloc is not yet
/// implemented, so `Tuple` sites are reported but not lowered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SiteKind {
    /// `Op::MakeRecord`: `shape_idx` is meaningful, `field_count` is
    /// the record's field count.
    Record,
    /// `Op::MakeTuple`: `shape_idx` is unused (`0`), `field_count` is
    /// the tuple arity.
    Tuple,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EscapeSite {
    pub pc: u32,
    pub kind: SiteKind,
    pub shape_idx: u32,
    pub field_count: u16,
    pub escapes: bool,
}

/// Analyze every function in `functions`. Returns one
/// `EscapeReport` per function that contains at least one
/// `MakeRecord` site (functions with no record allocations are
/// omitted — the consumer doesn't care about them).
pub fn analyze_program(functions: &[Function]) -> Vec<EscapeReport> {
    functions
        .iter()
        .filter_map(|f| {
            let r = analyze_function(f);
            (!r.sites.is_empty()).then_some(r)
        })
        .collect()
}

/// Analyze one function. Cheap to call even when there are no
/// record sites (early-exits after the first pass over `code`).
pub fn analyze_function(func: &Function) -> EscapeReport {
    // Inventory the MakeRecord sites first. If there are none,
    // skip the whole fixpoint — the function can't benefit from
    // stack allocation anyway.
    let sites: Vec<(u32, SiteKind, u32, u16)> = func
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, op)| match op {
            Op::MakeRecord { shape_idx, field_count } => {
                Some((pc as u32, SiteKind::Record, *shape_idx, *field_count))
            }
            Op::MakeTuple(arity) => {
                Some((pc as u32, SiteKind::Tuple, 0, *arity))
            }
            _ => None,
        })
        .collect();
    if sites.is_empty() {
        return EscapeReport { fn_name: func.name.clone(), sites: vec![] };
    }

    let n = func.code.len();
    let locals_count = func.locals_count as usize;
    let arity = func.arity as usize;

    // Per-pc in-states, computed by the fixpoint. None = unvisited.
    let mut in_state: Vec<Option<State>> = vec![None; n];
    let mut escaped: HashSet<u32> = HashSet::new();

    let mut worklist: Vec<(usize, State)> = vec![(0, State::entry(locals_count, arity))];

    while let Some((pc, incoming)) = worklist.pop() {
        if pc >= n { continue; }

        // Merge into existing in-state; only enqueue successors
        // when something actually changed (fixpoint termination).
        let (merged, new_escapes) = match &in_state[pc] {
            Some(prev) => {
                let (m, e) = prev.merge_with(&incoming);
                if &m == prev && e.is_empty() {
                    continue;
                }
                (m, e)
            }
            None => (incoming, HashSet::new()),
        };
        escaped.extend(new_escapes);
        in_state[pc] = Some(merged.clone());

        // Step the op, get the out-state + any successors.
        let (out, succs, leaked) = step(pc, &func.code[pc], merged);
        escaped.extend(leaked);
        for s in succs {
            if s < n {
                worklist.push((s, out.clone()));
            }
        }
    }

    let report_sites = sites
        .into_iter()
        .map(|(pc, kind, shape_idx, field_count)| EscapeSite {
            pc,
            kind,
            shape_idx,
            field_count,
            escapes: escaped.contains(&pc),
        })
        .collect();
    EscapeReport { fn_name: func.name.clone(), sites: report_sites }
}

/// Apply one op to the abstract state. Returns the new state, the
/// list of successor pcs (with their starting state being the
/// returned state, except for control-flow ops where successors
/// share the *same* state), and any sites that escaped during the
/// step.
fn step(pc: usize, op: &Op, mut s: State) -> (State, Vec<usize>, HashSet<u32>) {
    let mut escapes: HashSet<u32> = HashSet::new();

    // Helper closures for the common pop-n / push patterns.
    let leak = |slot: Slot, into: &mut HashSet<u32>| {
        if let Slot::Agg(p) = slot { into.insert(p); }
    };
    let pop_n_leak = |state: &mut State, n: usize, esc: &mut HashSet<u32>| {
        for _ in 0..n {
            if let Some(top) = state.stack.pop() { leak(top, esc); }
        }
    };
    let pop_n_drop = |state: &mut State, n: usize| {
        for _ in 0..n { state.stack.pop(); }
    };

    match op {
        Op::PushConst(_) => { s.stack.push(Slot::Other); }
        Op::Pop => { s.stack.pop(); /* drop — no escape on plain pop */ }
        Op::Dup => {
            // Aliasing breaks our linear-flow tracking. Anything
            // duplicated escapes — both copies become Other.
            if let Some(top) = s.stack.pop() {
                leak(top, &mut escapes);
                s.stack.push(Slot::Other);
                s.stack.push(Slot::Other);
            }
        }

        Op::LoadLocal(i) => {
            let slot = s.locals.get(*i as usize).copied().unwrap_or(Slot::Other);
            s.stack.push(slot);
        }
        Op::StoreLocal(i) => {
            if let Some(top) = s.stack.pop() {
                let i = *i as usize;
                if i >= s.locals.len() { s.locals.resize(i + 1, Slot::Other); }
                // Round-tripping an aggregate through a local keeps it
                // tracked. Overwriting a local that held a *different*
                // tracked aggregate does NOT make the old one escape:
                // Lex `let` bindings are immutable, so the compiler
                // only reuses a slot once its previous occupant is out
                // of scope (dead). Any real escape of that occupant —
                // returned, passed to a call, stored into another
                // aggregate — flows through a `Load → {Return, Call,
                // MakeRecord/MakeTuple/...}` chain those ops already
                // leak. Flagging it here was a pure false-positive
                // that defeated stack-alloc for the common
                // destructure-then-bind pattern (`compile_match`
                // rewinds its `__scrut`/`__tuple` temp slots, so the
                // enclosing `let` reuses them — see #464 tuple codegen).
                s.locals[i] = top;
            }
        }

        // Allocation site.
        Op::MakeRecord { field_count, .. } => {
            // Field values get stored in the new heap record; if
            // any of them is itself a tracked Rec, it escapes (now
            // referenced from inside the parent).
            pop_n_leak(&mut s, *field_count as usize, &mut escapes);
            s.stack.push(Slot::Agg(pc as u32));
        }
        // #464 step 2: post-lowering form of MakeRecord (escape
        // proved). Re-running the analysis on already-lowered code
        // must produce the same shape, so treat it identically.
        Op::AllocStackRecord { field_count, .. } => {
            pop_n_leak(&mut s, *field_count as usize, &mut escapes);
            s.stack.push(Slot::Agg(pc as u32));
        }
        // A tuple is a stack-allocatable aggregate, tracked the same
        // way as a record: its field operands leak (any tracked
        // aggregate stored inside it escapes), and the tuple itself
        // becomes a tracked candidate keyed on this pc. `GetElem`
        // reads an element without escaping the tuple, exactly as
        // `GetField` does for records.
        Op::MakeTuple(n) => {
            pop_n_leak(&mut s, *n as usize, &mut escapes);
            s.stack.push(Slot::Agg(pc as u32));
        }
        // Post-lowering form of MakeTuple (escape proved). Re-running
        // the analysis on already-lowered code must classify it the
        // same way, so treat it identically — mirrors AllocStackRecord.
        Op::AllocStackTuple { arity } => {
            pop_n_leak(&mut s, *arity as usize, &mut escapes);
            s.stack.push(Slot::Agg(pc as u32));
        }
        // Lists and variants remain escape sinks for any tracked
        // aggregate operand and don't create new tracked candidates —
        // list stack-allocation needs variable-length arena handling
        // (a later slice), and variants aren't a stack-alloc target.
        Op::MakeList(n) => {
            pop_n_leak(&mut s, *n as usize, &mut escapes);
            s.stack.push(Slot::Other);
        }
        Op::MakeVariant { arity, .. } => {
            pop_n_leak(&mut s, *arity as usize, &mut escapes);
            s.stack.push(Slot::Other);
        }
        Op::MakeClosure { capture_count, .. } => {
            pop_n_leak(&mut s, *capture_count as usize, &mut escapes);
            s.stack.push(Slot::Other);
        }

        // Field / element reads — receiver is consumed but only to
        // read a field. Doesn't escape; the receiver becomes
        // unreferenced after the op.
        Op::GetField { .. } => { s.stack.pop(); s.stack.push(Slot::Other); }
        Op::GetElem(_) => { s.stack.pop(); s.stack.push(Slot::Other); }
        Op::TestVariant(_) => { /* peek-only */ s.stack.pop(); s.stack.push(Slot::Other); }
        Op::GetVariant(_) => { s.stack.pop(); s.stack.push(Slot::Other); }
        Op::GetVariantArg(_) => { s.stack.pop(); s.stack.push(Slot::Other); }
        Op::GetListLen => { s.stack.pop(); s.stack.push(Slot::Other); }
        Op::GetListElem(_) => { s.stack.pop(); s.stack.push(Slot::Other); }
        Op::GetListElemDyn => {
            // pop [list, idx] → push elem
            s.stack.pop(); s.stack.pop(); s.stack.push(Slot::Other);
        }
        Op::ListAppend => {
            // pop [list, value]; value is now inside the list → escape
            if let Some(value) = s.stack.pop() { leak(value, &mut escapes); }
            s.stack.pop(); // list itself
            s.stack.push(Slot::Other);
        }

        // Control flow.
        Op::Jump(off) => {
            let target = (pc as i32 + 1 + off) as usize;
            return (s, vec![target], escapes);
        }
        Op::JumpIf(off) | Op::JumpIfNot(off) => {
            s.stack.pop(); // consumed Bool
            let target = (pc as i32 + 1 + off) as usize;
            return (s, vec![pc + 1, target], escapes);
        }
        Op::Return => {
            if let Some(top) = s.stack.pop() { leak(top, &mut escapes); }
            return (s, vec![], escapes);
        }
        Op::Panic(_) => {
            return (s, vec![], escapes);
        }
        Op::TailCall { arity, .. } => {
            pop_n_leak(&mut s, *arity as usize, &mut escapes);
            return (s, vec![], escapes);
        }
        Op::Call { arity, .. } => {
            pop_n_leak(&mut s, *arity as usize, &mut escapes);
            s.stack.push(Slot::Other);
        }
        Op::CallClosure { arity, .. } => {
            // pop arity args + 1 closure
            pop_n_leak(&mut s, *arity as usize + 1, &mut escapes);
            s.stack.push(Slot::Other);
        }
        Op::SortByKey { .. } | Op::ParallelMap { .. } => {
            // pop [xs, f]; both escape
            pop_n_leak(&mut s, 2, &mut escapes);
            s.stack.push(Slot::Other);
        }
        Op::EffectCall { arity, .. } => {
            pop_n_leak(&mut s, *arity as usize, &mut escapes);
            s.stack.push(Slot::Other);
        }

        // Pure arithmetic / comparison / logic / string ops. Their
        // operands are primitives in well-typed code (the existing
        // type checker rejects record-typed args to IntAdd etc.),
        // so we don't expect Rec slots to flow in. If one does, the
        // pop_n_drop loses the Rec without recording escape — but
        // that would be a type-system bug surfaced elsewhere.
        Op::IntAdd | Op::IntSub | Op::IntMul | Op::IntDiv | Op::IntMod
        | Op::IntEq | Op::IntLt | Op::IntLe
        | Op::FloatAdd | Op::FloatSub | Op::FloatMul | Op::FloatDiv
        | Op::FloatEq | Op::FloatLt | Op::FloatLe
        | Op::NumAdd | Op::NumSub | Op::NumMul | Op::NumDiv | Op::NumMod
        | Op::NumEq | Op::NumLt | Op::NumLe
        | Op::BoolAnd | Op::BoolOr
        | Op::StrConcat | Op::StrEq | Op::BytesEq => {
            pop_n_drop(&mut s, 2);
            s.stack.push(Slot::Other);
        }
        Op::IntNeg | Op::FloatNeg | Op::NumNeg | Op::BoolNot
        | Op::StrLen | Op::BytesLen => {
            s.stack.pop();
            s.stack.push(Slot::Other);
        }

        // Superinstructions (#461). All operate on Int locals and
        // primitive stack values — they neither produce nor consume
        // Rec slots. The trailing tombstones are inert; the verifier
        // pattern (skip ahead by N) is mirrored here.
        Op::LoadLocalAddIntConst { .. } => {
            // +1 net (LoadLocal + PushConst + IntAdd)
            s.stack.push(Slot::Other);
        }
        Op::LoadLocalAddIntConstStoreLocal { dest, .. } => {
            // delta 0; updates a local with an Int. Overwriting a
            // local doesn't escape its prior aggregate — same rule as
            // plain StoreLocal above (the dest is Int-typed by this
            // superinstruction's contract, so prev is never an
            // aggregate in practice; relaxed for consistency).
            let i = *dest as usize;
            if i >= s.locals.len() { s.locals.resize(i + 1, Slot::Other); }
            s.locals[i] = Slot::Other;
            return (s, vec![pc + 4], escapes);
        }
        Op::LoadLocalAddLocal { .. }
        | Op::LoadLocalSubLocal { .. }
        | Op::LoadLocalMulLocal { .. } => {
            // +1 net (two LoadLocal + one binop)
            s.stack.push(Slot::Other);
            return (s, vec![pc + 3], escapes);
        }
        Op::LoadLocalGetField { .. } => {
            // #461 slice 9: equivalent to LoadLocal + GetField —
            // reads a field out of a local record (which does NOT
            // escape the receiver, matching plain GetField) and
            // pushes the field value (Other). Net delta +1; skip the
            // single tombstone (pc+2). (Escape analysis runs before
            // peephole, so this arm is exercised only if the analysis
            // is ever re-run on fused code; it's here for exhaustive
            // matching and forward-correctness.)
            s.stack.push(Slot::Other);
            return (s, vec![pc + 2], escapes);
        }
        Op::LoadLocalGetFieldAdd { .. }
        | Op::LoadLocalGetFieldSub { .. }
        | Op::LoadLocalGetFieldMul { .. } => {
            // #461 slice 7/8: net delta on the value stack is 0 (pops
            // prior Int top, pushes Int result). The receiver is read
            // from a local — the analysis tracks locals separately,
            // and reading a local doesn't escape its Rec slot (the
            // round-trip-through-local rule from StoreLocal applies
            // here too: the value stays referenced). We pop the
            // existing top (the accumulator) and push a fresh Other
            // (the result). Skip past the two tombstones.
            s.stack.pop();
            s.stack.push(Slot::Other);
            return (s, vec![pc + 3], escapes);
        }
        Op::LoadLocalEqIntConstJumpIfNot { jump_offset, .. } => {
            // delta 0; two successors (fall-through past tombstones,
            // and the branch target relative to the original
            // JumpIfNot's pc).
            let target = (pc as i32 + 4 + jump_offset) as usize;
            return (s, vec![pc + 4, target], escapes);
        }
        Op::LoadLocalStoreEqIntConstJumpIfNot { dst, jump_offset, .. } => {
            // delta 0; also writes locals[dst] := locals[src].
            // Treat the local update the same as StoreLocal of an
            // Other (the scrutinee is an Int per slice-6's contract).
            let i = *dst as usize;
            if i >= s.locals.len() { s.locals.resize(i + 1, Slot::Other); }
            // Overwriting a local doesn't escape its prior aggregate
            // (same rule as plain StoreLocal); dst is Int-typed here.
            s.locals[i] = Slot::Other;
            let target = (pc as i32 + 6 + jump_offset) as usize;
            return (s, vec![pc + 6, target], escapes);
        }
    }

    (s, vec![pc + 1], escapes)
}

/// Convenience wrapper over `analyze_program` returning a map
/// keyed by `(fn_name, pc)` for direct lookup during codegen.
pub fn build_escape_index(functions: &[Function]) -> HashMap<(String, u32), bool> {
    let mut idx = HashMap::new();
    for report in analyze_program(functions) {
        for site in report.sites {
            idx.insert((report.fn_name.clone(), site.pc), site.escapes);
        }
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Op;
    use crate::program::{Function, ZERO_BODY_HASH};

    /// Helper: build a minimal Function with the given code and
    /// just enough machinery for the analyzer.
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

    /// Expectation helper: a list of `(pc, expected_escapes)` pairs.
    fn assert_escapes(report: &EscapeReport, expected: &[(u32, bool)]) {
        let got: Vec<(u32, bool)> = report.sites.iter().map(|s| (s.pc, s.escapes)).collect();
        assert_eq!(got, expected,
            "escape report for `{}` differs from expected", report.fn_name);
    }

    #[test]
    fn record_built_and_dropped_does_not_escape() {
        // PushConst PushConst MakeRecord Pop Return
        let f = func("dropper", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, false)]);
    }

    #[test]
    fn record_returned_escapes() {
        let f = func("returner", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, true)]);
    }

    #[test]
    fn record_field_read_only_does_not_escape() {
        // PushConst PushConst MakeRecord GetField Return (returns the field, not the record)
        let f = func("reader", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::GetField { name_idx: 0, site_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, false)]);
    }

    #[test]
    fn record_round_tripped_through_local_does_not_escape() {
        let f = func("roundtrip", 1, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::StoreLocal(0),
            Op::LoadLocal(0),
            Op::GetField { name_idx: 0, site_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, false)]);
    }

    #[test]
    fn record_stored_into_outer_record_escapes() {
        // Build inner, then build outer with inner as one of its fields.
        let f = func("nest", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 }, // inner @ pc=2
            Op::PushConst(2),
            Op::MakeRecord { shape_idx: 1, field_count: 2 }, // outer @ pc=4
            Op::Return,                                       // outer escapes
        ]);
        let r = analyze_function(&f);
        // inner escapes (captured in outer); outer escapes (returned).
        assert_escapes(&r, &[(2, true), (4, true)]);
    }

    #[test]
    fn record_passed_to_call_escapes() {
        let f = func("passer", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, true)]);
    }

    #[test]
    fn record_captured_in_closure_escapes() {
        let f = func("capturer", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeRecord { shape_idx: 0, field_count: 2 },
            Op::MakeClosure { fn_id: 1, capture_count: 1 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, true)]);
    }

    #[test]
    fn record_in_one_branch_returned_escapes_after_merge() {
        // if cond { rec1 } else { rec2 } — Return after merge.
        // Conservative analysis: at the merge both sites escape.
        let f = func("brancher", 0, 1, vec![
            Op::LoadLocal(0),                          // pc=0
            Op::JumpIfNot(4),                          // pc=1; offset 4 → pc=6
            Op::PushConst(0),                          // pc=2
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // pc=3 (then-branch)
            Op::Jump(2),                               // pc=4; offset 2 → pc=7
            Op::PushConst(1),                          // pc=5 (unreached fall-through dead code)
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // pc=6 (else-branch)
            Op::Return,                                // pc=7 (merge + return)
        ]);
        let r = analyze_function(&f);
        // Both record sites escape — Return sees a merged stack.
        assert_escapes(&r, &[(3, true), (6, true)]);
    }

    #[test]
    fn two_sites_classified_independently() {
        // One record returned, one popped — they should classify
        // separately. Sequencing: build keeper, store it; build
        // discard, pop it; load keeper, return.
        let f = func("mixed", 1, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // keeper @ pc=1
            Op::StoreLocal(0),
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // discard @ pc=4
            Op::Pop,
            Op::LoadLocal(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, true), (4, false)]);
    }

    #[test]
    fn function_with_no_record_sites_produces_empty_report() {
        let f = func("pure_arith", 0, 2, vec![
            Op::LoadLocal(0),
            Op::LoadLocal(1),
            Op::IntAdd,
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert!(r.sites.is_empty());
    }

    #[test]
    fn analyze_program_skips_no_record_functions() {
        let f1 = func("noreds", 0, 0, vec![Op::PushConst(0), Op::Return]);
        let f2 = func("hasrec", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Return,
        ]);
        let reports = analyze_program(&[f1, f2]);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].fn_name, "hasrec");
    }

    #[test]
    fn record_passed_to_effect_call_escapes() {
        let f = func("effecting", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::EffectCall { kind_idx: 0, op_idx: 0, arity: 1, node_id_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, true)]);
    }

    #[test]
    fn record_duplicated_escapes() {
        // Dup is conservatively an escape — aliasing breaks the
        // linear-flow assumption.
        let f = func("duper", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Dup,
            Op::Pop,
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, true)]);
    }

    #[test]
    fn record_in_list_escapes() {
        let f = func("listed", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::MakeList(1),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, true)]);
    }

    #[test]
    fn build_escape_index_keys_by_fn_and_pc() {
        let f = func("idx_test", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 },
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let idx = build_escape_index(&[f]);
        assert_eq!(idx.get(&("idx_test".into(), 1)), Some(&false));
    }

    // ---- tuple widening (#464 tuple analysis slice) ----

    #[test]
    fn tuple_built_and_dropped_does_not_escape() {
        let f = func("tup_drop", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2), // pc=2
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, false)]);
        assert_eq!(r.sites[0].kind, SiteKind::Tuple);
        assert_eq!(r.sites[0].field_count, 2);
    }

    #[test]
    fn tuple_returned_escapes() {
        let f = func("tup_ret", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2), // pc=2
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, true)]);
    }

    #[test]
    fn tuple_elem_read_only_does_not_escape() {
        // Reading an element (GetElem) consumes the tuple locally,
        // mirroring GetField on a record — the tuple stays a candidate.
        let f = func("tup_read", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2), // pc=2
            Op::GetElem(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, false)]);
    }

    #[test]
    fn tuple_round_tripped_through_local_does_not_escape() {
        let f = func("tup_rt", 1, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2), // pc=2
            Op::StoreLocal(0),
            Op::LoadLocal(0),
            Op::GetElem(1),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, false)]);
    }

    #[test]
    fn tuple_passed_to_call_escapes() {
        let f = func("tup_call", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2), // pc=2
            Op::Call { fn_id: 1, arity: 1, node_id_idx: 0 },
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, true)]);
    }

    #[test]
    fn record_stored_into_tuple_escapes_tuple_stays_local() {
        // Build a record, wrap it in a tuple, drop the tuple. The
        // record escapes into the tuple; the tuple itself is local.
        let f = func("rec_in_tup", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // rec @ pc=1
            Op::MakeTuple(1),                                // tup @ pc=2 holds rec
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, true), (2, false)]);
    }

    #[test]
    fn tuple_stored_into_record_escapes() {
        let f = func("tup_in_rec", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2),                                // tup @ pc=2
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // rec @ pc=3 holds tup
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(2, true), (3, true)]);
    }

    #[test]
    fn tuple_and_record_sites_carry_distinct_kinds() {
        let f = func("mixed_kinds", 0, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 7, field_count: 1 }, // pc=1 record
            Op::Pop,
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2), // pc=5 tuple
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_eq!(r.sites.len(), 2);
        assert_eq!((r.sites[0].pc, r.sites[0].kind, r.sites[0].shape_idx), (1, SiteKind::Record, 7));
        assert_eq!((r.sites[1].pc, r.sites[1].kind, r.sites[1].field_count), (5, SiteKind::Tuple, 2));
        assert!(!r.sites[0].escapes && !r.sites[1].escapes);
    }

    #[test]
    fn aggregate_overwritten_in_dead_slot_does_not_escape() {
        // rec1 is stored into local 0, then local 0 is overwritten
        // with an Int and rec1 is never otherwise used. Lex's
        // immutable `let` bindings mean a slot is only reused once its
        // prior occupant is dead, so the overwrite must NOT flag rec1
        // as escaping (the relaxed StoreLocal rule — #464 tuple
        // codegen). Pre-relaxation this returned `true`.
        let f = func("overwrite", 1, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // rec1 @ pc=1
            Op::StoreLocal(0),
            Op::PushConst(0),
            Op::StoreLocal(0), // overwrite local 0 with an Int
            Op::PushConst(0),
            Op::Return,
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, false)]);
    }

    #[test]
    fn aggregate_loaded_then_returned_still_escapes() {
        // Soundness guard for the relaxed overwrite rule: an aggregate
        // stored in a local, then LOADED and returned, still escapes —
        // the `Return` leak catches it independent of any overwrite.
        let f = func("load_then_return", 1, 0, vec![
            Op::PushConst(0),
            Op::MakeRecord { shape_idx: 0, field_count: 1 }, // rec1 @ pc=1
            Op::StoreLocal(0),
            Op::LoadLocal(0),
            Op::Return, // returns rec1 → escapes
        ]);
        let r = analyze_function(&f);
        assert_escapes(&r, &[(1, true)]);
    }

    #[test]
    fn tuple_only_function_now_produces_report() {
        // Pre-widening this function had no tracked sites and was
        // omitted from analyze_program; now its tuple site is reported.
        let f = func("tuponly", 0, 0, vec![
            Op::PushConst(0),
            Op::PushConst(1),
            Op::MakeTuple(2),
            Op::Pop,
            Op::PushConst(0),
            Op::Return,
        ]);
        let reports = analyze_program(&[f]);
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].sites.len(), 1);
        assert_eq!(reports[0].sites[0].kind, SiteKind::Tuple);
    }
}
