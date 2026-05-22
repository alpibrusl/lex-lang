# Dispatch-overhead measurement (#461 next-phase go/no-go)

**Date:** 2026-05-18
**Branch:** `claude/issue-461-dispatch-profile` (data + bench only,
do not merge implementation changes)
**Question:** Before committing to a function-table / computed-goto
dispatch rewrite (the "next phase" of #461 named in the
ic-polymorphism writeup), measure how much of current VM time is
actually dispatch overhead after superinstruction slices 1 & 2
landed. If dispatch is no longer a meaningful fraction, the rewrite
is theatre.

## Method

1. Ran the existing dispatch benches on current main (release mode):
   `arith_loop`, `record_field`, `call_heavy`, `straight_arith`,
   `straight_arith_no_dispatch`.
2. Added a new bench `dispatch/pure` — a hand-built `Program` whose
   body is N × (`PushConst(Unit)` + `Pop`) followed by
   `PushConst(Int 0)` + `Return`. Two dispatches per iter, with the
   cheapest possible arm bodies (one `Vec::push(Value::Unit)`, one
   `Vec::pop()`). This bounds pure dispatch+trivial-arm cost from
   above.

## Results

```
dispatch/pure n=10000:                  136.87 µs  →  6.85 ns/op
dispatch/straight_arith       n=5000:    94.30 µs  →  4.72 ns/elem
dispatch/straight_arith_nd    n=5000:   154.91 µs  →  7.75 ns/elem
dispatch/arith_loop          n=10000:  1816.40 µs  →  ~18 ns/source-op
dispatch/record_field        n=10000:  7896.20 µs  →  ~790 ns/GetField
dispatch/call_heavy             n=20:    10.14 µs  →  ~500 ns/Call
```

`straight_arith` reports source ops (4 per let-binding); after
superinstructions, 4 source ops collapse to 1 dispatched op, so
4.7 ns/elem ≈ 1.2 ns dispatched-op rate at the source level.

## Decomposition

Pure dispatch is 6.85 ns/op with trivial arms. Vec::push of
`Value::Unit` + Vec::pop costs about 1-2 ns of actual work (small,
no heap, hot in cache). The remaining **~5 ns/op is dispatch
overhead**: PC fetch, match-arm decode, step-counter increment,
step-limit check, frame indirection.

Cross-checking against `straight_arith` vs `no_dispatch`:
- no_dispatch: 7.75 ns/elem doing 4 source-op equivalents inline →
  ~1.9 ns/elem of pure arm work
- bytecode: 4.72 ns/elem doing 1 superinstruction = 4 source-op
  equivalents → 4.72 ns/superinstruction = dispatch + larger arm
  body (IntAdd + LoadLocal + StoreLocal inlined)
- Bytecode wins the head-to-head because the superinstruction's arm
  body skips two stack roundtrips that no_dispatch still pays.

## Dispatch-rewrite upside (per workload)

If a function-table or computed-goto rewrite halves dispatch
overhead (6 ns → 3 ns — optimistic for Rust without inline asm):

| workload | ns/op | dispatch component | upside if halved |
|---|---:|---:|---:|
| straight_arith (superinstr) | 4.72 | 1.2 ns | ~12% (1.2 → 0.6) |
| arith_loop | ~18 | ~6 ns | ~16% |
| pure-dispatch microbench | 6.85 | ~5 ns | ~36% |
| record_field (IC + IndexMap) | 790 | ~6 ns | <0.5% |
| call_heavy (frame setup) | ~500 | ~10 ns | <2% |

Realistic average across mixed workloads: **5-15%.** Maximum on
pathological dispatch-bound code: 35-40%.

## Recommendation

**Skip the function-table dispatch rewrite for now. Pivot to #461
slice 3 (more superinstructions).**

Reasons:
1. **Diminishing-returns curve favors superinstructions.**
   Slice 1: +37%, slice 2: +72% on top of slice 1 (3.35× cumulative
   on the canonical bench). The next slice attacks the largest
   remaining cost (arm-body work + cross-op stack traffic), not
   dispatch. Same code surface as previous slices.

2. **Dispatch rewrite ROI is bounded.** Best-case 35-40% on
   dispatch-bound microbenches, but realistic 5-15% on mixed
   workloads. It's also an invasive change: rewriting the
   1500-line `run_to()` match into a function table touches every
   op, complicates debugging (stack traces lose meaningful frames),
   and may pessimize cold paths (function calls vs inlined arms).

3. **`straight_arith` already beats no-dispatch floor.** The
   canonical dispatch microbench shows bytecode at 4.72 ns/elem vs
   no-dispatch at 7.75 ns/elem — 1.64× faster. Whatever dispatch
   overhead remains, the comparison-of-record everyone looks at
   says we've already won. Spending implementation budget here is
   not the highest-value move.

## Candidate slice 3 patterns

A quick grep for compiled-code patterns in the existing apps would
identify the next fusion candidate. Strong candidates:

- `LoadLocal + IntLt + JumpIfNot` — loop-condition idiom, fires
  every iteration of `while` / tail-recursive `match n { 0 => ...;
  _ => ... }`.
- `LoadLocal + LoadLocal + IntAdd` — binary-op-on-two-locals, fires
  in any `a + b` expression where both are locals.
- `PushConst + IntEq + JumpIfNot` — pattern-match arm test for
  small int literals (the `match n { 0 => ... }` head case in
  every recursive function).

Slice 2 already proved the peephole-pass pattern; slice 3 is
plumbing on top of it.

## Reproducing

```sh
git checkout claude/issue-461-dispatch-profile
cargo bench -p lex-bytecode --bench dispatch -- --quick
```

`dispatch/pure/n=10000` is the new bench. The other five are
unchanged from main.

## Update 2026-05-22 — recommendation reversed to GO

The "skip the rewrite" recommendation above was correct **for the
workloads it measured** (arithmetic microbenches), but it is now
stale. New data flips it.

### What changed

The decomposition above measured *arith* shapes, where
superinstructions had already collapsed 4 source ops into 1 dispatch
and `straight_arith` beat the no-dispatch floor. On those shapes the
residual dispatch upside really is 5–15%, and skipping was right.

But the workload that matters for real Lex deployments is
*record-heavy* (HTTP handlers, SQL row decoders), and there
superinstructions don't collapse the op stream the same way. The
2026-05-21 callgrind profile of `response_build` (in #461's issue
comment), taken after slices 5–9, the memo-key rework (#529), and
adaptive memoization (#532) all landed, shows:

```
21.58M (48.0%)  Vm::run_to        <- dispatch (this issue)
 4.45M ( 9.9%)  drop_in_place<Value>
 3.24M ( 7.2%)  <Value as Clone>::clone
```

`run_to` is **48% of I-refs** on a record-heavy workload. The other
levers that used to compete with dispatch (memo hashing, value
clone) have been knocked down by the recent perf arc, so dispatch is
now the single largest remaining cost — exactly the condition under
which this rewrite stops being theatre.

### Revised recommendation: **GO**, but bench-gated and sliced

Caveat that the original doc's skepticism still holds in part: a
naïve `fn(&mut Vm)` handler table can *regress* in Rust, because the
current `match` over a small `Op` enum already lowers to a jump
table and inlines arm bodies, whereas a function-pointer table adds
an indirect call and de-inlines. So the rewrite is **not** "swap the
match for a table" — it is the set of changes that actually remove
per-op work:

1. **Slice A — hoist per-op loop invariants.** Today the prologue
   re-derives `frame_idx = frames.len()-1`, re-borrows
   `code = &program.functions[fn_id].code`, and re-checks
   `pc >= code.len()` on *every* op. `fn_id`/`code` only change at
   Call / CallClosure / TailCall / Return. Restructure `run_to` into
   an outer (per-frame) / inner (per-op) loop so the code slice is
   fetched once per frame-entry, not once per op. The borrow-checker
   constraint (can't hold `&[Op]` across `&mut self` arm bodies) is
   the real design decision: candidates are `Function.code:
   Arc<[Op]>` (clone the Arc per frame-entry — one refcount bump per
   Call, not per op) or a contained `unsafe` raw-slice pointer
   refreshed at the 5 frame-transition sites. **Note INVARIANTS.md:**
   confirm an `Arc<[Op]>` change does not perturb bytecode
   serialization / SigId before committing to it.
2. **Slice B — drop verifier-discharged checks.** The
   `pc >= code.len()` guard is redundant given #366 (the slice index
   panics on OOB anyway); demote to `debug_assert!`. Small, but free.
3. **Slice C (optional, feature-flagged) — threaded dispatch.**
   computed-goto via `asm!` or tail-call `become` once stable. This
   is where the C-interpreter literature's gains actually live; keep
   behind `#[cfg(feature = "computed_goto")]` with the match loop as
   the portable default, same as #461 originally proposed.

Each slice re-runs `cargo bench -p lex-bytecode --bench dispatch`
**and** the `response_build` bench before/after, and `cargo test
--workspace` must stay green — same discipline as superinstruction
slices 1–9. Slice A is the one with the measured 48% behind it;
B and C are follow-ons.

### Slice A landed — measured result (2026-05-22)

First increment of slice A: cache the executing function's code
slice (`program.functions[fn_id].code`) across ops, re-resolving
only when `fn_id` changes (the frame-transition set). Works because
`program` is a borrowed `&'a Program` — the slice reference is
independent of the `&mut self` the op handlers take, so it can live
across the dispatch arms with no `unsafe`, no `Arc`, no serialization
change. `frame_idx` / `fn_id` stay recomputed per op, so the 70+ op
handlers are untouched.

Measured with the deterministic tool, not wall-clock: criterion on
this shared VM has a ~4–5% run-to-run noise floor (the
`straight_arith_no_dispatch` control — native Rust that never enters
`run_to` — drifts that much between back-to-back runs), which swamps
a slice-A-sized effect. callgrind I-refs are drift-immune, so they
are the gate for changes this small. `profile_response_build 120 3`:

| | I-refs | `run_to` I-refs |
|---|---|---|
| before | 11,073,057 | 4,344,448 (39.2%) |
| after  | 10,946,101 | 4,206,062 (38.4%) |
| Δ | **−1.15%** total | **−3.2%** in `run_to` |

A modest, real reduction in the dominant VM function, in the
single-digit range the table above predicted. The larger win still
lives in slice C (threaded dispatch); slice A's lasting value is
that the cached code slice is the structural precondition for it.
`cargo test -p lex-bytecode` is green; `--workspace` could not be
run to completion in the dev container (disk: each lex-runtime
integration binary statically links the full polars/arrow/tokio
graph and the set overflows the volume), but the reentrant-path
coverage (`list_sort_by`) and the full bytecode suite pass.
