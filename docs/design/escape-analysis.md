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
  before `Return` — under `Policy::FrameScope` both escape at the
  `Return` (`Return` leaks every site in the merged set, same
  result as the pre-refinement collapse-to-Other). Under
  `Policy::RequestScope` neither escapes — the join's `AggSet([p,q])`
  passes through `Return` without leaking. This was previously
  listed as future work; the precision refinement landed via #463
  follow-up (see `arena-plumbing.md` § "Status update (2026-06-03)").

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

## Step 2 — `AllocStackRecord` + polymorphic `GetField`

The step-2 slice (this PR's sibling commits) lowers proven-local
`MakeRecord` sites to a new opcode `Op::AllocStackRecord` and a
new `Value` variant `Value::StackRecord { shape_id, slab_start,
field_count }`. The slab lives in a VM-wide
`stack_record_arena: Vec<Value>` truncated on every `Op::Return`,
mirroring the `locals_storage` lifetime discipline from #389.

### What we did NOT add (vs the future-work table)

Initial design notes mentioned `GetStackField` / `SetStackField`
opcodes. We dropped both:

- **`SetStackField`**: Lex records are immutable — there is no
  existing `SetField` op for either heap or stack records, and the
  AST has no field-assignment syntax. The opcode would have nothing
  to lower from.

- **`GetStackField`**: `Op::GetField` already dispatches over the
  `Value::Record` variant via an inline-cache slot keyed by
  `(fn_id, site_idx)` and verified by `(shape_id, offset)`. Stack
  records carry the same `shape_id` (issued from
  `Program::record_shapes`) and store their fields in shape order
  (matching `MakeRecord`'s IndexMap insertion order), so the
  existing IC slot is interoperable — one new match arm in the
  `GetField` handler replaces what would have been a whole new op.
  A dedicated `GetStackField` is still on the table as a peephole
  optimization once we have a static "this GetField receiver was
  just stack-allocated" pass, but it would compete on dispatch
  overhead at the µs scale and is deferred.

### Budget

Per-frame budget pinned at `STACK_RECORD_BUDGET_SLOTS = 64` Value
cells (= 4 KiB at the current `size_of::<Value>() == 64`). The
budget is tracked on `Frame::stack_record_budget_remaining` and
checked inside the VM at every `AllocStackRecord`; on overflow
the op falls back to the heap path (an exact MakeRecord) with no
user-visible difference. A `TailCall` refills the budget — the
tail-called function gets its own arena view.

### What step 2 ships

- `Op::AllocStackRecord { shape_idx, field_count }` (op.rs)
- `Value::StackRecord { shape_id, slab_start, field_count }` (value.rs)
- VM arena (`stack_record_arena`) with per-frame start markers and
  budget (vm.rs)
- Compiler pass `apply_escape_lowering` (compiler.rs) — consults
  `escape::build_escape_index` and rewrites non-escaping
  `MakeRecord` sites in place
- Body-hash invariance: `AllocStackRecord` decodes as the legacy
  `MakeRecord` form (#222), so closure identity survives the
  lowering bit-identically

## Future work

| Issue | Scope                                                | When              |
|-------|------------------------------------------------------|-------------------|
| (new) | Per-path branch refinement (recover the `if/else` merge case) | If profiling shows it matters |
| (new) | Inter-procedural escape via summaries on small leaf functions | After inlining (#465 phase 1) |
| (new) | `GetStackField` peephole — drop the variant-match on receiver when the producer is a same-fn `AllocStackRecord` | If dispatch shows up in the response_build profile |

## Acceptance

### Step 1 (#524 — merged)

- [x] `analyze_program` returns one `EscapeReport` per function
  with at least one `MakeRecord` site.
- [x] All 15 lattice unit tests pass.
- [x] No regression on `cargo test -p lex-bytecode --tests`.
- [x] `cargo clippy -p lex-bytecode --all-targets -- -D warnings`
  clean.

### Step 2 (#525 — merged)

- [x] `Op::AllocStackRecord` round-trips through verifier,
  body-hash, and serde (the latter via the existing `Op` derive).
- [x] Compiler lowers exactly the non-escaping `MakeRecord` sites
  per the per-PC escape index.
- [x] Per-frame budget falls back to heap with identical observable
  output when exhausted.
- [x] Polymorphic `GetField` dispatches over both `Value::Record`
  and `Value::StackRecord` with shared IC slot.
- [x] 9 new integration tests in
  `crates/lex-bytecode/tests/stack_records.rs` pass.
- [x] `cargo test -p lex-bytecode` (75 tests), `-p lex-trace`,
  `-p lex-runtime --lib` (46), `-p core-compiler --test m9/m9_phase2`
  (19), and runtime integration `std_http` / `analytics_app` /
  `closed_pydantic_issues` / `conc_registry` / `arena_lifecycle`
  all pass.
- [x] `cargo clippy -p lex-bytecode --all-targets -- -D warnings`
  clean.

### Step 3 (this PR) — `response_build` bench + #464 acceptance

- [x] `benches/response_build.rs` (criterion) compares enabled
  vs disabled lowering on a 6-intermediate-records-per-call handler
  shape. Measured speedup: **2.84–2.94×** at n ∈ {100, 1000},
  well above the issue's ≥1.5× bar.
- [x] `tests/response_build_acceptance.rs` exact stack-allocation
  rate: **85.71%** (1200 stack / 200 heap / 0 fallback per drive(200)),
  above the ≥60% bar.
- [x] `LEX_NO_STACK_RECORDS=1` toggle on the lowering pass so the
  bench can A/B identical source under matched VM conditions.
- [x] Per-VM counters `Vm::stack_record_allocs` /
  `heap_record_allocs` / `stack_record_heap_fallbacks` for the
  rate measurement.
- [x] `cargo test -p lex-bytecode` (76 tests; the new ignored
  timing test brings the total to 77) passes; release-mode
  `--ignored` timing test reports 2.96× and clears its 1.3×
  regression floor.
- [x] `cargo clippy -p lex-bytecode --all-targets -- -D warnings`
  clean.

#### Why the speedup is bigger than the bar

The issue's ≥1.5× bar was a conservative estimate. The workload's
6 non-escaping records per handler call exercise the
`Box<IndexMap>` allocation path 6× per iter on the disabled arm —
each one allocating an `IndexMap` (entries vec + indices hashmap)
plus the `Box` itself. The enabled arm's stack path is a single
`Vec::resize` per record into the per-frame arena, dropped
wholesale on `Op::Return`. Skipping 6 mallocs per call is what
pushes the ratio toward 3×; a thinner workload (~2 intermediates
per call) lands closer to the 1.5× bar.

#### Why the in-CI timing assertion is at 1.3× not 1.5×

Wall-clock comparisons on shared CI runners are noisy by ~15-20%
on the loser arm; a tight 1.5× gate would flap. The deterministic
acceptance (stack rate) is the strong gate; the timing test is a
secondary regression check at a relaxed threshold. The criterion
bench produces the publishable number.
