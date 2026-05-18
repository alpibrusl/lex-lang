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
