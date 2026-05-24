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
