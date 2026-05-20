# Escape analysis for `MakeRecord` sites (#464)

**Date:** 2026-05-20
**Status:** Design doc accompanying step 1's implementation
(`crates/lex-bytecode/src/escape.rs`). Steps 2 and 3 are still
pending — this doc is the spec they will be reviewed against.

## Why this doc

#464 lands in three slices: analysis (this PR), opcodes + compiler
integration (next), bench + acceptance (last). The slices are small
on their own but make architectural commitments — the abstract
lattice, the op classification table, the spill story — that the
later slices inherit. Writing the contract down before steps 2 and 3
means the lattice can be challenged independently of the codegen
work.

## Goal

Statically prove, per `Op::MakeRecord` site, that the resulting
record value never leaves the function frame on any reachable path.
A site that satisfies this can be lowered to a frame-local
allocation (a contiguous run of locals owned by the caller's
`call_frame`) instead of the current heap path through
`Box<IndexMap<String, Value>>`.

Out of scope: `MakeList`, `MakeTuple`, `MakeVariant`,
`MakeClosure`. Lists and tuples have their own representations
that would need separate machinery; closures are heap-allocated
by design (their captures may be shared across worker handlers).

## What counts as "escape"

A record allocated at pc `P` escapes if any reachable path uses it
as an operand of one of:

| Consumer                                | Why it escapes                                   |
|-----------------------------------------|--------------------------------------------------|
| `Return` / `TailCall`                   | Crosses the frame boundary into the caller.      |
| `Call` / `CallClosure` / `EffectCall`   | Crosses into a callee we can't see (intra-only). |
| `MakeRecord` (as a field value)         | Becomes a field of a heap-allocated parent.      |
| `MakeList` / `MakeTuple` / `MakeVariant`| Becomes an element / arg of a heap aggregate.    |
| `MakeClosure` (as a capture)            | Becomes part of a closure that may itself escape.|
| `SortByKey` / `ParallelMap`             | Captured by the worker-pool closure.             |
| `ListAppend` (as the appended value)    | Stored inside the (heap) list.                   |
| `Dup`                                   | Aliasing — conservatively both copies escape.    |
| Join-point merge with `Other` or `Rec(q≠p)` | Lost track of the site past the merge.       |
| `StoreLocal(i)` overwriting a different `Rec(q)` | `q` may still be on the stack — flag.   |

Local-only consumers (do **not** cause escape):

- `GetField` / `GetElem` / `TestVariant` / `GetVariant` /
  `GetVariantArg` / `GetListLen` / `GetListElem` /
  `GetListElemDyn` — read-only operations that produce a derived
  value (we don't track that derived value's escape, since it's
  a field, not the record itself).
- `Pop` — drops without observing.
- `StoreLocal(i)` / `LoadLocal(i)` round-trip — the record stays
  tracked across the spill.
- Arithmetic / comparison / boolean / string ops — operands are
  primitives by well-typedness; if a record slot reaches them it
  is a type-system bug surfaced elsewhere.
- Superinstructions (#461 slices 1–6) — operate on Int locals
  and primitive stack values; analysis mirrors the verifier's
  tombstone-skip pattern.

## Abstract lattice

```
Slot ::= Rec(pc)  -- record allocated by MakeRecord at this pc, still local
       | Other    -- anything else (primitives, escaped records, opaque values)
```

Join `⊔` is pointwise per slot:

```
Rec(p) ⊔ Rec(p) = Rec(p)
Rec(p) ⊔ Rec(q) = Other     -- (and both p, q escape)
Rec(p) ⊔ Other  = Other     -- (and p escapes)
Other  ⊔ Other  = Other
```

A function-level abstract state is `(stack: Vec<Slot>,
locals: Vec<Slot>)`. Worklist fixpoint over the CFG: each pc has
an in-state; merging a new incoming state into it that produces
either (a) a change to the merged state or (b) any new escape
recordings enqueues the successors with the post-op out-state.
Termination: the lattice is finite (at most one `Rec(pc)` per
`MakeRecord` site, plus `Other`) and merges are monotone toward
`Other` — no cycles.

## What this analysis does NOT prove

**Stack overflow safety.** A site that's marked non-escaping may
still blow the host stack if the function nests many such records.
Step 2 of #464 will impose a per-frame byte budget (initial
proposal: 4 KiB total stack-record bytes per `call_frame`).
Records past the budget fall back to heap allocation — runtime
fallback, not an analysis-level rejection — so a single function
can mix stack and heap records.

**Borrow checking.** The analysis treats `StoreLocal` /
`LoadLocal` round-trips as identity-preserving, which is correct
under the current ref-count-free Value model but would need
revisiting if Lex ever introduces interior mutability into
records.

**Inter-procedural escape.** A record passed to a pure helper
(e.g. `format_response(r: Response) -> Str`) is flagged as
escaping at the `Call` site. Inlining is the standard fix and is
deliberately deferred — see "Future work."

## Why intra-procedural

Acceptable per the #464 issue's wording ("function frame"), and
matches the immediate win we care about: handler functions like

```lex
fn handle(req: Request) -> Response {
  Response { status: 200, body: render(req.body), headers: ... }
}
```

build the response in a straight line, return it, and that's it
— no helper indirection on the allocation path. Inter-procedural
analysis would catch the `format_response`-style cases but
doubles–triples implementation complexity (call-graph,
summarization, recursive cases) and the JIT roadmap (#465) gets
us cross-fn elision for free once inlining lands.

## Soundness contract

Step 2 of #464 will treat this analysis as a **necessary** but
not sufficient precondition for emitting `AllocStackRecord`:

- An *over-approximation* (false positive — flagged as escaping
  when it doesn't) costs a heap allocation per request. That's
  the existing baseline; acceptable.
- An *under-approximation* (false negative — flagged as local
  when it actually escapes) would let the runtime stack-record
  outlive its frame and cause UB.

Mitigation: step 2 pairs the analysis with an unconditional
runtime fallback — if the stack-record slot the frame owns runs
out (size budget, recursion depth, …), the codegen falls back to
heap allocation. So a missed escape still costs correctness only
if the analysis claims a record is local *and* the codegen
omits the fallback — never both at once.

## Status quo lattice precision (step 1)

The implementation pinned by `escape::tests`:

- Single straight-line return of a fresh record → escapes. ✓
- Fresh record dropped or only field-read → does NOT escape. ✓
- Fresh record round-tripped through one local → does NOT escape. ✓
- Two MakeRecord sites in the same function classified
  independently. ✓
- Records nested inside another record (the inner site escapes
  via capture by the outer aggregate). ✓
- Records passed to `Call` / `EffectCall` / `MakeClosure` /
  `MakeList`. ✓
- Records duplicated via `Dup`. ✓
- Records produced in alternate `if/else` branches and merged
  before `Return` — both escape at the join. (Conservative; a
  per-path refinement could recover this but is out of scope.)

## API surface

```rust
pub mod escape {
    pub fn analyze_program(&[Function]) -> Vec<EscapeReport>;
    pub fn analyze_function(&Function) -> EscapeReport;
    pub fn build_escape_index(&[Function]) -> HashMap<(String, u32), bool>;

    pub struct EscapeReport { pub fn_name: String, pub sites: Vec<EscapeSite> }
    pub struct EscapeSite   { pub pc: u32, pub shape_idx: u32,
                              pub field_count: u16, pub escapes: bool }
}
```

Re-exported as `lex_bytecode::{analyze_escapes, EscapeReport,
EscapeSite}`. Step 2's compiler integration will call
`build_escape_index` once per program and consult it at each
`MakeRecord` emit site.

## Future work

| Issue | Scope                                                | When              |
|-------|------------------------------------------------------|-------------------|
| #464 step 2 | `AllocStackRecord`, `GetStackField`, `SetStackField`; compiler integration; per-frame stack-record budget | Next slice |
| #464 step 3 | `benches/response_build.rs`; 1.5× + 60% acceptance | After step 2 |
| (new) | Per-path branch refinement (recover the `if/else` merge case) | If profiling shows it matters |
| (new) | Inter-procedural escape via summaries on small leaf functions | After inlining (#465 phase 1) |

## Acceptance for this slice (step 1)

- [x] `analyze_program` returns one `EscapeReport` per function
  with at least one `MakeRecord` site.
- [x] All 15 lattice unit tests pass.
- [x] No regression on `cargo test -p lex-bytecode --tests` (70
  passing).
- [x] `cargo clippy -p lex-bytecode --all-targets -- -D warnings`
  clean.

Steps 2 and 3 carry the full #464 acceptance bars (≥1.5× speedup
on `response_build`, ≥60% of `Response` allocations on the stack).
