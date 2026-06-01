# Plumbing the per-request arena into the VM (#463 scoping)

**Date:** 2026-05-24
**Status:** Scoping doc — **no implementation yet**. Picks the
architecture and slices the work so #463 can be picked up without
re-deriving the design. Companion to `escape-analysis.md` (#464),
which this work generalizes, and `jit-roadmap.md` (#465), which
lists #463 as a phase-0 prereq.

## Why this doc

#463 ("per-request arena allocator tied to effect scope") has had
its lifecycle scaffolding landed (`crates/lex-runtime/src/arena.rs`
+ `enter_request_scope` / `exit_request_scope` on the `EffectHandler`
trait, wired into `net.serve_fn`'s request loop) but **routes zero
allocations** — the arena is created and dropped per request as a
no-op. The 2026-05-21 `response_build` profile on #461 puts the
allocation churn this targets (`drop_in_place<Value>` + libc `free`)
at **~15% of I-refs**, the second-largest cost after dispatch and the
highest-ROI lever now that the cheap dispatch slices (A+B, −3.4%) are
spent. This doc decides *how* to route allocations and where the real
risk lives, before any code is written.

## Goal

Allocate request-scoped value trees (the issue names `Response`, the
headers `Map`, JSON values from validators) so that the *whole tree*
is freed in one bulk operation at response time instead of running a
`drop_in_place` + `free` per node. Lifetime = the request handler's
`[net_*]` effect scope, which the lifecycle scaffolding already opens
and closes at the right boundaries.

Non-goal for the first landable increment: a fully general
inter-procedural escape solver. See "Slicing" — slice 1 is the risk
and should ship the narrowest analysis that pays.

## What already exists (and what it tells us)

Two facts from the current tree decide most of the architecture:

1. **#464 already solved value-routing without a lifetime param on
   `Value`.** `Value::StackRecord { shape_id, slab_start, field_count }`
   and `Value::StackTuple { slab_start, arity }` are **POD handles** —
   indices into a flat `Vm::stack_record_arena: Vec<Value>` that is
   bulk-truncated on `Op::Return`, with a per-frame 64-slot budget and
   an unconditional heap fallback on overflow. No `unsafe`, no `Drop`
   dispatch, no churn at the ~60 `as_int()` call sites. The arena work
   is, in large part, **generalizing this slab+handle mechanism from
   frame-scope to request-scope.**

2. **The byte-bump `Arena` in `arena.rs` is the wrong tool for the
   slab+handle route, and the route that *would* use it is
   toolchain-blocked.** `Arena::alloc(len) -> &mut [u8]` is shaped to
   back the heap parts of today's `Value` (`Box<IndexMap>`,
   `VecDeque<Value>`, `Vec<u8>`) *in place* — i.e. give those
   collections a custom allocator. But custom allocators for
   `Box`/`Vec`/`IndexMap` need the **`allocator_api`, which is unstable
   on the project's pinned stable toolchain** (the same nightly wall
   that blocks #461 slice C's `become`/computed-goto). So the
   "bump-allocate the existing collections" route is not viable on
   stable today.

**Conclusion: the viable architecture on stable is Route A (slab +
handle), generalizing #464 — not Route B (allocator-backed
collections, nightly-gated).** The byte-bump `arena.rs` should be
treated as either repurposed into a `Vec<Value>` request slab or
retired; it is not load-bearing for Route A.

## The two routes, explicitly

| | Route A — slab + handle (generalize #464) | Route B — allocator-backed collections |
|---|---|---|
| Mechanism | New handle variants index a request-scoped `Vec<Value>` slab, bulk-dropped at `exit_request_scope`. | `Value::Record`/`List` keep their shape; their `IndexMap`/`VecDeque` backing is allocated from a bump arena. |
| `Value` churn | New variants only; the ~60 `as_int()`-style sites untouched (same as #464). | Lifetime param `Value<'a>` **or** allocator tag + custom `Drop` — months of churn per `arena.rs`'s own note. |
| Toolchain | **Safe stable Rust** (it is how #464 already works). | Needs `allocator_api` — **nightly-only on the pinned toolchain.** |
| Frees the leaves? | Only if the *whole* subtree is arena-routed (see "deep-leaf trap"). | Yes — backing store is bump-freed regardless of nesting. |
| Verdict | **Recommended.** | Blocked on stable; revisit only if the toolchain pin changes. |

## The real risk: escape *scope*, not the routing mechanism

#464's escape analysis (`escape.rs`) proves **frame-local** — "this
record never leaves its function frame." The arena needs a
**request-local** proof — "this value never leaves the request
scope." These are different questions, and #464's analysis answers
the wrong one for us: it conservatively flags everything crossing
`Return` / `Call` as escaping, which is exactly the value (the
`Response` returned up through the handler) the arena most wants to
capture. This is the "design risk" the issue itself names ("arena
lifetime tied to effect scope is the design risk").

A general request-scope proof is **inter-procedural** (it must follow
the value across the whole handler call tree), which `jit-roadmap.md`
defers until inlining lands (#465 phase 1). Doing it bottom-up like
#464 is a large project. So slice 1 should **invert the framing**:

> **Top-down, not bottom-up.** The issue's own rationale — "every
> value allocated inside a `[net_*]`-effected handler frame is
> arena-eligible *by construction*" — means the default is *arena*,
> and the analysis's job is to find the **escape hatches out of the
> request scope** and exclude only those. The hatches are a small,
> enumerable set: `spawn` / `ParallelMap` / channel-send (value
> outlives the request on a worker thread), closure captures stored
> in module-level / global state, and the pure-fn memo cache
> (`vm.rs:146`, outlives every request). Everything built in the
> `[net_*]` frame that touches none of these is arena-routable.

This is both more tractable than a bottom-up lattice and a better fit
for the workload. It inherits #464's **soundness contract verbatim**:
an over-approximation (heap a value that could've been arena'd) costs
the status-quo allocation — fine; an under-approximation (arena a
value that escapes the request) is UB — *must* be impossible, and is
backstopped the same way #464 backstops StackRecord: an unconditional
runtime fallback to the heap path, so a missed hatch costs correctness
only if the analysis is wrong *and* the fallback is omitted, never
both.

## Slicing (mirrors #464's analysis → opcodes → bench cadence)

- **Slice 0 — lifecycle scaffolding. DONE.** `arena_stack`,
  `enter/exit_request_scope`, wired into `net.serve_fn`;
  `tests/arena_lifecycle.rs` proves nesting/pairing symmetry.
- **Slice 1 — request-scope escape analysis (the risk; ship narrow).**
  Top-down "arena-by-default, exclude the hatches" pass over handler
  functions. Start with the single-function handler shape the issue
  and `escape-analysis.md` both center on; treat any `Call` into a
  non-inlined helper as a hatch initially (i.e. intra-procedural,
  conservative), and widen only if the profile shows helper-built
  response fragments dominating. Land behind a `build_arena_index`
  API mirroring `build_escape_index`, with the same unit-test-pinned
  lattice discipline.
- **Slice 2 — opcode + codegen routing.** Generalize the #464 slab:
  either new handle variants (`ArenaRecord`/`ArenaList`/`ArenaTuple`)
  indexing a request-scoped `Vec<Value>`, or a scope-tag on the
  existing Stack* handles. Lowering pass consults slice 1; runtime
  fallback to heap on any uncertainty. Body-hash invariance the same
  way #464 got it: the new op decodes as its legacy `MakeRecord`/
  `MakeList` form (#222) so closure identity survives bit-identically
  — **no bytecode-format change** (a hard acceptance bar).
- **Slice 3 — bench + acceptance.** `bench/alloc_heavy.lex`, p99 on
  `bench/floor.lex /json`, per-VM `arena_allocs` / `heap_allocs` /
  `arena_heap_fallbacks` counters for a deterministic rate gate (as
  #464 used a stack-alloc-rate gate alongside the noisy wall-clock).

## Risks and constraints to design in from day one

- **Deep-leaf trap.** Route A only removes a `free` if the *entire*
  subtree dying at request end is arena-routed. A `List` whose spine
  is an arena handle but whose elements are heap `Record`s still runs
  a per-element `drop`/`free`. The #461 prototype already showed this
  is *the* failure mode (linear-use trees, refcount-1, the cost is in
  the leaves). So slice 1/2 must cover the dominant constructors
  *together* (Record + List + Tuple, and the `IndexMap` field-name
  keys / `Vec<u8>` bytes underneath) — a half-routed tree banks little.
- **Arena handles MUST be readable at serialization — unlike
  StackRecord.** `Value::StackRecord` uses a panic-guard shortcut at
  `to_json` / equality / memo because the analysis guarantees it never
  reaches those (`value.rs:362`). Arena values are the opposite: the
  `Response` *is* serialized (`to_json`) before the scope closes, so
  arena handles need **real read support** at those boundaries, not a
  panic guard. This is strictly more work than StackRecord and is the
  main place Route A is not a free copy-paste of #464.
- **Worker-thread lifetime split.** `spawn_for_worker` clone-handlers
  get a *fresh empty* `arena_stack` by design (`handler.rs:83-95`),
  because worker allocations outlive the spawning request. Any value
  handed to a worker (`spawn`/`ParallelMap`/channel) must therefore be
  a heap value, never an arena handle — this is one of the slice-1
  hatches, and getting it wrong is a dangling-slab UB.
- **Nested scopes.** `arena_stack` already supports nesting. Either
  tag each handle with its `ScopeId` (so reads after an inner pop are
  caught) or restrict allocation to the innermost scope and forbid
  cross-scope handles (simplest; matches #464's one-arena-per-frame).
  Decide in slice 2.
- **Pure-fn memo.** The memo cache outlives requests; arena values
  must never enter it (the issue says pure fns stay on the global
  allocator). The hatch set must exclude pure-fn allocation sites —
  cheap, since the compiler already knows which functions are pure.
- **Testing under the disk cap.** `cargo test --workspace` cannot run
  to completion in the dev container (each lex-runtime integration
  binary statically links the full polars/arrow/tokio graph and the
  set overflows the volume — same constraint #464 hit). Gate on
  `-p lex-bytecode`, `-p lex-runtime --lib`, and the targeted
  `arena_lifecycle` / `std_http` integration binaries.

## Acceptance (from #463, restated)

- [ ] ≥ 2× speedup on a new alloc-heavy bench (`bench/alloc_heavy.lex`).
- [ ] p99 latency on `bench/floor.lex /json` drops measurably (no
  per-node free on the hot path).
- [ ] No regression on the runnable test set; attestation
  reproducibility preserved.
- [ ] **No bytecode-format change** (new ops decode as their legacy
  `MakeRecord`/`MakeList` form, #222).

## One-paragraph recommendation

Take **Route A** (generalize #464's slab+handle into a request-scoped
`Vec<Value>` dropped at `exit_request_scope`); it is the only route
that works on the pinned stable toolchain and reuses a proven, safe
mechanism. Spend the risk budget on **slice 1's request-scope
analysis**, framed **top-down** ("arena by default inside `[net_*]`,
exclude the enumerable escape hatches") rather than as a bottom-up
inter-procedural solver. Design **serialization-readable arena
handles** and the **worker-thread hatch** in from the start — those
are the two places Route A genuinely diverges from #464. Repurpose or
retire the byte-bump `arena.rs`; it backs the toolchain-blocked Route
B, not the path we can ship.

## Status update (2026-06-02) — slice 2b-i landed; measured

All four planned slices are now on `main` (#579 analysis, #588 ops +
slab, #589 boundary helper, #590 codegen lowering + runtime
wire-up). Callgrind measurement on the two workloads we care about:

### `alloc_heavy` (simple single-record-return handler)

`examples/profile_alloc_heavy.rs` — `drive(1000)` × 5 iters. The
handler is `fn handle(i) -> Response { {status: 200, total: i*2,
count: i+1} }`. Single allocation site per call, no `match` in the
return position, so the slice-1 analysis classifies it as arena-
eligible and `apply_arena_lowering` rewrites it to
`AllocArenaRecord`.

| | I-refs | Δ |
|---|---|---|
| arena off (LEX_NO_ARENA_RECORDS=1 or no scope) | 40,078,370 | — |
| arena on (scope wraps `vm.call("drive")`) | 25,656,712 | **−36.0%** |

Where the savings land:

|  | off | on | Δ |
|---|---|---|---|
| `_int_malloc` | 2.92M (7.28%) | 0.82M (3.19%) | **−71.9%** |
| `_int_free` | 2.05M (5.10%) | 0.71M (2.75%) | **−65.5%** |
| `malloc` | 1.43M (3.58%) | 0.49M (1.92%) | **−65.7%** |
| `free` | 0.89M (2.22%) | 0.30M (1.18%) | **−65.9%** |
| `IndexMap::insert_full` | 1.58M (3.93%) | 0 | **gone** |
| `Vec::resize` | 0.71M (1.77%) | 1.18M (4.58%) | slab growth |
| `Vm::run_to` | 13.57M | 12.08M | −11.0% |

The per-record `Box<IndexMap>` malloc/free pair vanishes — replaced
by bulk slab writes (`Vec::resize`) and bulk truncation on
`exit_request_scope`. This is the alloc-churn lever the #461 profile
identified at ~15% of total I-refs on a record-heavy workload.

### `response_build` (the original `#461`-profile workload)

`examples/profile_response_build.rs`. The handler returns a
`Response` built inside a two-arm `match`:

```lex
match v6.s > 0 {
  true  => { status: 200, total: v6.s + v6.t + v6.u },
  false => { status: 400, total: 0 },
}
```

| | I-refs |
|---|---|
| arena off | 11,005,530 |
| arena on  | 11,009,096 |

**Indistinguishable (Δ < 0.1%).** The arena pass **fires zero times**
on this workload — `[diag] handle() record sites: arena=0, stack=6,
heap=2`. The 6 intermediates (v1..v6) hit the cheaper #464 stack
tier; the two `match`-arm `MakeRecord` sites are flagged as escaping
by slice-1's lattice because the join point merges `Slot::Rec(p)`
and `Slot::Rec(q)` to `Slot::Other` (both sites then recorded as
escapes).

This is the **"if/else merge" conservative case** already explicitly
documented in `escape-analysis.md` § "Status quo lattice precision"
(carries over verbatim under the request-scope policy):

> "Records produced in alternate `if/else` branches and merged
> before `Return` — both escape at the join. (Conservative; a
> per-path refinement could recover this but is out of scope.)"

So the analysis is doing exactly what its spec says, and the
workload's shape (which is representative — handlers commonly
return one of several response shapes via match) is precisely the
case it conservatively rejects.

### What this means for next work

The honest read across both measurements:

1. **The arena machinery works** and delivers a clean ~36% I-ref
   reduction on the workload shape it covers.
2. **The slice-1 analysis is the next lever.** A per-path refinement
   that tracks `Slot::Rec(pc)` separately along each branch (instead
   of merging to `Other` at joins) would let `response_build`'s
   two-arm `match` classify both branches as arena-eligible, since
   neither leaks under the request-scope policy. This is a pure
   analysis change — the codegen and runtime already handle two
   independent `AllocArenaRecord` sites in a function (slice 2b-i
   tests cover the per-site classification).
3. **The "deep-leaf" widening** (children whose only hatch is the
   outer's `MakeRecord`) is a sibling case, also pure analysis.
   Both would land via a single slice that adds two precision
   refinements to `arena::analyze_function_with_policy`.

Neither refinement requires any change to the runtime, codegen, or
serialization paths landed in slices 2a–2b. The work is contained.

Out of immediate scope: the materialize-at-boundary step (slice 2a-iii)
does pay a non-trivial walk + alloc, and shows up as ~9% of the on-
arm in `drop_in_place` and ~4.6% in `Vec::resize`. A future
**slab-direct serializer** (walk arena handles → JSON bytes without
ever materializing into `Value::Record`) would recover that cost on
the HTTP-serving path. Not on the critical path until the analysis
refinements above land and the response-build shape sees the arena
fire.

## Status update (2026-06-03) — per-path precision refinement landed

The "next lever" from the 2026-06-02 measurement above (track
`Slot::Rec(pc)` separately along each branch instead of merging to
`Slot::Other` at joins) is now on `main`. `escape::Slot` gained a
3-variant `Agg(pc) | AggSet(Vec<u32>) | Other` shape; `Slot::merge`
unions site sets across joins; `State::merge_with` no longer records
escapes at the merge itself; the leak helper in `step()` iterates
`Slot::sites()` so multi-site slots leak every member at genuine
escape ops (`Call` / `EffectCall` / `MakeClosure` / etc.).

Soundness: under `Policy::FrameScope` (the #464 default), `Op::Return`
still leaks every site in the merged set, so the frame-escape result
is bit-identical to pre-refinement. Under `Policy::RequestScope`
(#463), `Op::Return` is the only consumer whose answer changes — the
merge set passes through it without leaking, and only request-leaking
hatches downstream cause an escape.

### `response_build` re-measured (same harness as 2026-06-02)

`examples/profile_response_build.rs`, `drive(120)` × 3 iters,
callgrind I-refs.

| | I-refs | Δ vs OFF |
|---|---|---|
| arena off (LEX_NO_ARENA_RECORDS=1 / no scope) | 11,228,628 | — |
| arena on  (precision pass + scope) | **10,023,352** | **−10.7%** |

Where the savings land:

| | off | on | Δ abs |
|---|---|---|---|
| `_int_malloc` | 4.92% | 3.43% | **−37.7%** |
| `_int_free` | 3.37% | 2.50% | **−33.8%** |
| `malloc` | 2.50% | 1.90% | **−32.1%** |
| `free` | 1.47% | 1.08% | **−34.2%** |
| `IndexMap::insert_full` | 1.00% | 0.38% | **−66.5%** |
| `Vm::run_to` | 37.05% | 40.73% | (relative — abs ~unchanged) |

Diagnostic shows `[diag] handle() record sites: arena=2, stack=6,
heap=0` (was `arena=0, stack=6, heap=2` before the refinement). Both
`match`-arm `Response` records now land on the arena tier; runtime
counters confirm `arena_allocs=360, arena_fallbacks=0` across the
3 × 120 calls.

### What's still open

- **Deep-leaf widening** — `MakeRecord` still pessimistically leaks
  every field operand at the build site, so a record stored as a
  field of another record continues to escape. Sibling to the join
  precision; would be a second refinement to the lattice that
  threads parent-eligibility through nested aggregates.
- **Slab-direct serializer** — the materialize-at-boundary cost
  (`drop_in_place` + `Vec::resize` in the materialize walk) is still
  paid. A walker that emits JSON bytes directly out of `arena_slab`
  would shed it on the HTTP-serving path.

## Status update (2026-06-04) — deep-leaf widening landed

The sibling refinement from the 2026-06-03 status. The slice-1
analysis used to leak every field operand at each tracked aggregate's
build site (`MakeRecord` / `MakeTuple` / `AllocStack*` /
`AllocArena*`), so a record stored as a field of another record
escaped immediately — even when the outer itself stayed local.

The fix is a **containment-tracking** refinement: at each build site,
the analysis records the popped children as *contained in* the
parent pc rather than escaping them. After the fixpoint, an
expansion pass transitively escapes the children of every escaped
parent. Net effect:

- **Outer escapes → all children escape transitively** (sound — same
  result as before).
- **Outer stays local → children stay local** (precision win — the
  pre-refinement analysis pessimistically over-flagged).

### Deep-tree workload — the new measurement

`examples/profile_deep_tree.rs` — a 3-deep nested record per call,
`drive(1000) × 3 iters`. All three levels arena-eligible only under
deep-leaf widening; pre-refinement only the outer was eligible (and
even then was leaked by the slice-1 lattice for `match`-shape
returns, fixed by the 2026-06-03 precision pass).

| | I-refs | Δ vs OFF |
|---|---|---|
| arena off | 40,939,025 | — |
| arena on  | **18,404,779** | **−55.0%** |

| | off | on | Δ abs |
|---|---|---|---|
| `_int_malloc` | 10.67% | 2.98% | **−87.5%** |
| `_int_free` | 7.01% | 2.47% | **−84.2%** |
| `malloc` | 4.93% | 1.74% | **−84.2%** |
| `IndexMap::insert_full` | 4.62% | gone | **−100%** |

Diagnostic: `[diag] handle() record sites: arena=3, stack=0,
heap=0`; runtime counters confirm 9000 arena allocs / 0 fallbacks
across 3 × 1000 calls × 3 levels per call.

### Headline summary across the arena work

| Workload | Shape | Arena win |
|---|---|---|
| `alloc_heavy`     | flat return record | **−36.0%** |
| `response_build`  | match-arm return    | **−10.7%** |
| `deep_tree`       | 3-deep nested      | **−55.0%** |

### What's still open

- **Slab-direct serializer** — the materialize-at-boundary cost
  (`drop_in_place` + `Vec::resize` in the materialize walk) is still
  paid. The deep-tree numbers above already pay it (the relative
  share of `drop_in_place` actually grew, from 3.42% to 9.73%, even
  though absolute went down — total I-refs shrank faster). A walker
  that emits JSON bytes directly out of `arena_slab` without
  rebuilding a `Value::Record` mirror would shed it on the
  HTTP-serving path.
