# JIT roadmap (#465 architectural decisions)

**Date:** 2026-05-20
**Status:** Design doc — closes the architectural-decisions
acceptance criterion of #465. No implementation yet.

> **Re-scope 2026-05-22 — see [Status update](#status-update-2026-05-22--re-scope)
> at the bottom.** Two measurements that landed after this doc was
> written change the phase-0 priority order: (1) the dispatch-loop
> rewrite (#461) is promoted from "deferred" to *next development*,
> and (2) the value-rep / NaN-boxing rework is demoted off the JIT
> critical path pending a JIT-specific measurement. The original
> analysis below is left intact; the re-scope section records what
> changed and why.

## Why this doc

#465 is the long-horizon JIT tracking issue. It explicitly lists
its acceptance as:

> - All prerequisites (#461, #462, #463, #464, value-rep) complete
> - Architectural decisions resolved in a design doc under
>   `docs/design/`
> - Scope re-evaluated based on post-#461-#464 bench numbers
> - Then: re-file with concrete deliverables

This doc handles the second bullet — picks provisional answers
to the open decisions so when the value-rep rework lands, the
JIT work has a frame to fit into instead of starting from a
blank page. The numbers in this doc are from the actual bench
suite (`crates/lex-bytecode/benches/dispatch.rs`); the
recommendations are pending re-evaluation once #463 / #464 /
value-rep are done, per the issue.

## Where the interpreter sits today

After six superinstruction slices (#461 series) and two #462
inline-cache slices:

| Bench (n=10000 or n=5000)         | Time      | Notes |
|-----------------------------------|-----------|-------|
| `arith_loop / n=10000`            | 2.10 ms   | Down from 2.63 ms pre-slice-5 (1.25×). TailCall-dominated; further gains need frame-setup work, not dispatch fusion. |
| `straight_arith / n=5000`         | 109 µs    | Beats the no-dispatch native-Rust counterpart (192 µs) 1.76× — dispatch is below the per-op floor on this shape. |
| `straight_arith_no_dispatch / n=5000` | 192 µs    | "What if we removed dispatch entirely" baseline. |
| `pure / n=10000`                  | 151 µs / 132 Melem/s | Bare dispatch floor: `PushConst + Pop` pairs. |
| `record_field_wide / n=10000`     | 58.92 ms  | IC keyed on shape_id (#517). |
| `record_field_dynamic / n=10000`  | 58.80 ms  | Dynamic records carry registry-interned shape_ids (#518). |
| `two_local_arith / n=5000`        | 137 µs    | Slice 3 (`LoadLocalAddLocal`). |
| `two_local_sub_arith / n=5000`    | 139 µs    | Slice 4. |
| `two_local_mul_arith / n=5000`    | 137 µs    | Slice 4. |

Effective per-op cost on `straight_arith` is 5.5 ns/op. The pure
dispatch floor is 7.6 ns/op. Slices 1–6 absorb the dispatch
boundary itself on the hot patterns. Diminishing returns from
this point on the interpreter side.

## What the interpreter ceiling is

JIT pays off when the residual overhead is no longer dispatch
but native arithmetic + memory traffic + branch prediction. The
benches above suggest the ceiling sits near `straight_arith_no_dispatch`'s
192 µs — a hand-written native-Rust loop in `Value`'s current
representation. That's where naïve JIT lands too: same boxed
`Value`s, same heap-allocated `IndexMap`, same per-op pattern
match in handlers — only the dispatch is replaced.

The **interesting** JIT ceiling is what happens *after* the
value-rep rework: NaN-boxed or tagged-pointer `Value`s let the
JIT inline `Value::as_int()` to a single mask, eliminate the
match-on-variant, and unbox arithmetic into raw registers. That's
where 3–5× speedups live. Without the value-rep rework, the JIT
caps near 1.5× — a lot of engineering for a small win.

**This is why #465 lists the value-rep rework as a hidden prereq.**

## Open architectural decisions and recommendations

### 1. Backend choice

Three live candidates.

**Cranelift** (used by Wasmtime, recent Spidermonkey backend,
several Lua/JS JITs):
- Mature, stable, MIT-licensed, pure Rust.
- One-pass codegen is fast (≤ 1 ms per function for typical
  Lex function sizes).
- IR (CLIF) is well-documented; lowering bytecode → CLIF is
  ~mechanical for the arithmetic/locals/jumps subset.
- Built-in support for relocations, exception-handler
  metadata, deopt side tables.
- Drawbacks: roughly 10 MB of generated code at build time;
  binary size hit. Not built for in-place patching (no template
  JIT semantics).

**Custom one-pass template** (LuaJIT-style for the
non-tracing case, or e.g. wasm3's interpreter-style):
- Smaller; we control register allocation hot paths.
- We re-implement deopt, stack maps, calling conventions.
- Estimated 3–6× more engineering than Cranelift.

**Compile to WASM, run via Wasmtime**:
- Reuses an entire stack we already pull in for
  `lex-frame` / `lex-runtime`'s WASM-target work (if that
  ever lands; today no WASM dep).
- Loses the JIT's ability to specialize per-call-site (you'd
  need module-per-function instantiation, which is slow).
- Cute but doesn't actually fit Lex's shape.

**Recommendation: Cranelift.** Mature, no Cranelift-specific
work blocks Lex feature work, and we can swap it out if
benchmarks ever justify going custom. The 10 MB compile-time
hit is acceptable; we already ship arrow + polars which dwarf
it. Reconsider only if benchmarks show Cranelift's one-pass
output is worse than a hand-tuned template by ≥ 30%.

### 2. Baseline-only vs. tiered

- **Baseline-only**: every function gets one JIT compilation,
  no speculation, no deopt. Roughly 4–6 months. Expected gain
  after value-rep rework: 2–3× on `arith_loop`-shaped workloads,
  1.2–1.5× on record-heavy.
- **Tiered with speculation + deopt**: interpreter →
  cold-baseline → hot-optimizing. Type-specialized,
  speculation-on-shape-ids. +12–18 months. Expected gain: 3–6×
  on the same workloads; Go-tier on tight loops.

**Recommendation: ship baseline-only first.** Reasons:
1. Tiered with deopt requires production-grade side tables
   (frame layout maps, register-vs-stack location records,
   bytecode-pc-from-native-pc resolution). That's where most of
   the engineering effort goes — not the codegen.
2. Lex's hot-path real workloads are HTTP handlers and SQL
   row decoders; both spend most of their time in stdlib
   builtins (`arrow_*`, `polars`, `tokio` syscalls), not in Lex
   bytecode. Baseline JIT recovers the dispatch+box cost on
   Lex code; tiered specialization recovers arm-body
   sub-microsecond cost. The dispatch+box cost is the bigger
   share on real workloads.
3. Tiered remains an extension. Adding optimizing tier later
   doesn't invalidate baseline output; baseline becomes the
   "cold" tier.

### 3. Content-addressed native code caching

> Should compiled native code be cached on disk keyed by
> bytecode SigId and shared across runs?

**Recommendation: yes, but as phase 2.** The motivation is
purely Lex-flavored: bytecode is already content-addressed
(SigId, body_hash). If two runs see the same body_hash, the
JITed code is bit-identical (modulo Cranelift version), and
caching skips compile time on warm starts. Wire it in once
phase-1 baseline JIT works.

Format: `~/.cache/lex/jit/<bytecode-version>/<body_hash>.so`
(or `.machO`, `.wasm`, …). One file per function. Loaded
with `dlopen` (or platform equivalent); JIT machine code
mapped read-execute. Safe because Lex bytecode body_hash
already pins the source + canonical form + verifier-pass
status; a hash collision would mean a bytecode collision and
the existing closure-identity machinery (#222) already
trusts it.

This makes Lex one of the very few content-addressed JITs.
A natural fit — and probably a noteworthy talking point.

### 4. Deopt strategy + NodeId sidecar

Native frames must map back to bytecode state for two reasons:

1. **Trace recorder.** `crates/lex-trace/src/recorder.rs:32`
   is NodeId-keyed. Every JIT entry into native code needs to
   carry the originating NodeId so the recorder can attribute
   events correctly.
2. **Runtime error reporting.** `VmError` carries `fn_name` +
   binding context (`crates/lex-bytecode/src/vm.rs:30-36`).
   Native code that hits an `expect("Int")` panic — say, type
   speculation failed — has to surface as the right
   `VmError::TypeMismatch` with the right NodeId, not a raw
   Rust panic.

**Recommendation: side tables, NodeId-tagged regions.**
Concretely:

- Each compiled function carries a `Vec<NodeIdMapping>` keyed
  on native-PC offset. On error or trace event, the runtime
  binary-searches into this vec to find the active NodeId.
- Speculation failures (phase 2+) emit deopt blobs that write
  the spilled register state back into the interpreter's stack
  + locals, then resume in the interpreter at the recorded
  bytecode PC. The bytecode PC is part of the same side table.
- Side-table format is a Lex-controlled binary blob, content-
  addressed alongside the native code so cache invalidation is
  trivial.

This is invariant 5 from #465 ("Trace recorder is NodeId-keyed
… JIT-safe iff compiled regions carry a NodeId metadata
sidecar — must be designed in from day one"). It's designed in.

## The value-rep rework (the hidden prereq)

The current `Value` enum:
- 64 bytes per `Value` (large enum)
- `Record { fields: Box<IndexMap<String, Value>> }` — heap walk
  for every field access
- `List(VecDeque<Value>)` — heap-allocated, pointer-chase
- `Str(SmolStr)` — already inline-optimized; fine
- `Closure { captures: Vec<Value> }` — heap-allocated, fine
  for now

**For JIT to be worth it, arithmetic Values (`Int`, `Float`,
`Bool`, `Unit`) need to be unboxed in the common case.** Two
classic approaches:

**NaN-boxing**: encode all non-Float Values in the NaN payload
of an f64. 51 bits available, tagged by exponent. Float values
are themselves, anything else is a tagged 51-bit payload.
Field-access still pointer-chases for records/lists/strings,
but arithmetic is a single u64 mask + compare.

**Tagged-pointer**: each Value is `(tag: u8, payload: u64)` or
a 64-bit packed `Box`-like with low-bit tags. Less elegant
than NaN-boxing but works on platforms where NaN-encoding is
ABI-unfriendly (some embedded targets we don't care about
yet).

**Recommendation: NaN-boxing**, gated behind a feature flag
during the transition. Land as **a new tracking issue, #525
(provisional)** — distinct from #465 because it's a 2–3 month
project on its own that touches every `as_int()` /
`as_float()` / `as_bool()` call site in the codebase (~60
sites per `git grep '\.as_int()' crates/`).

The flag lets the interpreter run both representations during
the migration so we can land changes incrementally.

## Phasing plan

After this doc lands, the work order is:

| Phase | Issue | Scope | Effort |
|-------|-------|-------|--------|
| 0 | #463 | Per-request arena tied to effect scope | 6–8 wk |
| 0 | #464 | Stack-allocate small records (escape analysis) | 4–5 wk |
| 0 | new   | Value rep rework — NaN-boxing | 2–3 mo |
| 1 | #465 phase 1 | Baseline JIT (Cranelift, arithmetic + locals + jumps + tail calls, interp fallback for records/closures/effects) | 4–6 mo |
| 2 | #465 phase 2 | Content-addressed native code cache; extend to records/closures | 2–3 mo |
| 3 | #465 phase 3 | Type specialization on shape_ids | 4–6 mo |
| 4 | #465 phase 4 | Tiered + deopt — Go-tier perf | 12+ mo |

Phase 1 alone delivers the milestone that matters: Lex
bytecode runs at native-Rust-loop speed for arithmetic-heavy
workloads. Anything past phase 2 is a polish question
re-evaluated on real production traces.

## Scope re-evaluation triggers

Per #465's acceptance criterion, scope should be re-evaluated
once the prereqs land. Specifically:

- If `bench/floor.lex /plaintext` shows the bottleneck is
  arrow/polars (not Lex code), JIT phase 1's ROI shrinks —
  prioritize arena (#463) instead.
- If post-NaN-boxing benches show the interpreter at ≥ 50%
  of native-Rust-loop speed already, baseline JIT delivers
  ≤ 2× and phase 1 should be scoped down to "type
  specialization only, no Cranelift" — much smaller project.
- If a production trace measures > 30% time in pattern-match
  arm tests, slice 7+ superinstructions might out-ROI JIT
  on real workloads. Re-bench before committing.

The goal of this doc is not to lock in a path; it's to make
the path's shape concrete enough that the prereq work can
build toward it without flailing.

## Invariants the JIT must preserve (from #465 verbatim)

For completeness, restating the architectural invariants that
the audit in #465 already confirmed JIT-safe:

- Attestations produced at `lex check` time, not VM-time.
- `examples {}` blocks validated at check time, not at runtime.
- Effect rows enforced statically; effect handler trampoline
  remains the runtime dispatch point.
- Trace recorder is NodeId-keyed — **must** carry NodeId
  sidecar (decided above).
- Runtime errors carry `fn_name` + binding context, not PC.
- Pure-fn memoization keyed on `(fn_id, sha256(args))`.

No Lex concept needs to be sacrificed. The cost is engineering,
not language design.

## Status update (2026-05-22) — re-scope

Per #465's "scope re-evaluation triggers" (above), the prereq order
is revised based on two measurements that landed *after* this doc.

### Phase-0 prereq scorecard

| Prereq | Issue | State (2026-05-22) |
|--------|-------|--------------------|
| Dispatch | #461 | **Partial** — superinstruction slices 1–9 + typed lowering + field-name interning landed; the **function-table / computed-goto dispatch-loop rewrite itself is not done**. |
| Inline caches | #462 | **Effectively done** — `(fn_id, site_idx)` key, `shape_id` IC, dynamic-shape interning. Polymorphic slice 2b **skipped**: measured 0% polymorphism (`ic-polymorphism-measurement.md`). |
| Per-request arena | #463 | **Scaffolding only** — `lex-runtime/src/arena.rs` exists but is not plumbed into the VM. |
| Stack-alloc records | #464 | **Done / closed.** `AllocStackRecord` + escape-driven lowering. |
| Value-rep (NaN-boxing) | new | **Not started — and de-prioritized, see below.** |

### What changed since 2026-05-20

**1. Dispatch is the dominant cost again on real workloads.** The
2026-05-18 go/no-go (`dispatch-overhead-measurement.md`) recommended
*skipping* the function-table rewrite — but that call was made on
*arithmetic* microbenches, where superinstructions already beat the
no-dispatch floor. The 2026-05-21 callgrind profile of
`response_build` (a *record-heavy* workload, representative of real
HTTP/SQL handlers) puts `Vm::run_to` back at **~48% of I-refs** after
all recent perf work landed. On the workloads that matter, dispatch
is once again the single largest cost — and it is a hard JIT prereq.
The earlier "skip it" decision is therefore **stale**; the rewrite is
promoted to next.

**2. Value-rep's interpreter-throughput ROI is ~3%, not the enabler
of 3–5×.** This doc (and #465/#480) treated NaN-boxing as the hidden
make-or-break prereq. A throwaway prototype (recorded in #461's
2026-05-21 comment) measured the cheapest version — `Arc`-wrapping
`Record.fields` — at **−2.8%**, with `drop_in_place<Value>`
*unchanged*. The churn is **linear-use** value trees: built, read
once, dropped, so refcount stays 1 and NaN-boxing only shrinks the
move/envelope cost, not the build/teardown of the underlying maps.
Estimated NaN-boxing upside on record-heavy interpreter throughput:
**~3%.**

This does **not** mean value-rep is worthless for the JIT — the
original §"value-rep rework" argument is about the JIT inlining
`as_int()` to a mask and unboxing arithmetic into native registers,
which is a *different* win than interpreter throughput. But that
JIT-specific ROI is **unmeasured**, and the interpreter-throughput
case that this doc leaned on is now falsified. So: NaN-boxing comes
**off the critical path** until its JIT-specific win is measured
(prototype: a hand-lowered unboxed arith loop vs the boxed one). Do
not start the 2–3 month rework on the strength of the old assumption.

### Revised next-development order

1. **#461 — function-table / computed-goto dispatch-loop rewrite.**
   Highest value on three independent axes: biggest single
   interpreter win (~48% of time; ~5 ns/op pure dispatch overhead),
   a hard JIT prereq, and no preconditions of its own. Caveat for
   implementers: a naïve `fn(&mut Vm)` table can *regress* in Rust
   (indirect call + non-inlined arms vs. the current `match`, which
   already lowers to a jump table). The genuine levers are (a)
   hoisting per-op loop invariants out of the prologue
   (`code`-slice refetch, `frame_idx` recompute, bounds checks that
   the verifier already discharges) and (b) threaded dispatch
   (computed-goto / tail-call `become`), the latter behind a
   feature flag. **Re-confirm with the dispatch bench before and
   after each slice** — same discipline as slices 1–9.
2. **#463 — plumb the per-request arena + widen #464 escape
   analysis** (lists/tuples/deeper nesting). Targets the ~15% in
   `drop_in_place` + libc `free` directly. Bigger, riskier change
   (lifetime tied to effect scope), so it follows the dispatch work.
3. **Value-rep / NaN-boxing — measure first, then decide.** Blocked
   on a JIT-specific micro-measurement (see above), not scheduled.

What explicitly should **not** be next: NaN-boxing (de-motivated),
or #462 slice 2b polymorphic ICs (0% measured polymorphism).

---

## Status update — MVP JIT lands (2026-06-01)

**A first-version JIT now exists in `crates/lex-jit`.** It is a
proof-of-concept, not a tier — the VM dispatcher is unchanged and
no caller routes through it. The goal was narrow: prove the
bytecode → CLIF → native-code → call-back pipeline works on the
constrained op set we'd plausibly start with.

### Scope

Supported ops: `PushConst(Int|Bool)`, `Pop`, `LoadLocal`,
`StoreLocal`, all integer arithmetic (`IntAdd/Sub/Mul/Div/Mod/Neg`),
integer comparisons (`IntEq/Lt/Le`), boolean logic
(`BoolAnd/Or/Not`), structured control flow (`Jump`, `JumpIf`,
`JumpIfNot`), and `Return`. Arity capped at 6 to keep the
trampoline table finite. Every Lex value at the JIT boundary is an
`i64` — `Int` flows unchanged, `Bool` as `0`/`1`. Anything else
(records, lists, closures, calls, effects, floats, strings,
superinstructions) makes the function ineligible via
`is_jit_eligible`.

### Architecture

Two-pass lowering:

1. `scan_blocks` walks the op stream to find basic-block entries
   (pc 0, every jump target, every pc after a jump) and computes
   the abstract-interpretation stack height at each entry.
2. `Lowering::run` walks ops sequentially, emitting CLIF into the
   current `Block`. Block-entry stack values are threaded through
   CLIF `block_params`; jumps pass the current SSA stack as
   block-call args. Locals are CLIF `Variable`s — Cranelift's SSA
   frontend handles φ-nodes for joins.

Cranelift is gated behind the `cranelift` cargo feature
(default-off), so stable builds don't pull cranelift's ~30 crates
into release artifacts.

### Verification

`tests/jit_vs_interpreter.rs` builds hand-crafted `Function`s,
runs each via the bytecode VM and via the JIT, and asserts the
results match across input batteries. 13 tests covering:

- straight-line int arithmetic (add, mul-add, const pool, locals,
  div/mod/neg);
- boolean logic with comparisons;
- forward conditional jumps (abs, max);
- a backward-jump loop (sum 1..n);
- rejection of unsupported ops / unsupported consts / overlarge
  arity via the eligibility gate.

### What this does *not* prove

- **Perf.** No benchmarks. The MVP at this op set may well be
  slower than the interpreter end-to-end once you count compile
  cost — that's expected; the work for measurable wins is the
  phase-1 deliverables below.
- **Integration.** The JIT is not wired into the VM dispatcher;
  there is no tier-up trigger, no fallback path, no NodeId
  side table for deopt, no error mapping back to `VmError`.
- **Value rep.** Records / closures / strings remain unsupported.
  That's the wall the MVP hits — every interesting Lex function
  uses at least one of them. Unboxing requires either the
  value-rep rework (NaN-boxing) or a runtime calling convention
  that boxes/unboxes at the JIT boundary.

### Concrete next steps if we want to keep going

In rough order of value:

1. **Wire it into the VM as an opt-in tier.** Add a `JitTier` on
   `Vm` that, on first `invoke`, calls `is_jit_eligible` and (if
   yes) compiles + caches by `body_hash`. Dispatch `Op::Call` to
   the JIT pointer when the cache hits; fall back to the
   interpreter otherwise. This is the minimum required to put
   real Lex programs through the JIT and measure anything.
2. **Add a side-by-side perf bench** (extend `benches/dispatch.rs`)
   that runs the same arith-loop both ways. Without measured
   ROI, all further JIT work is speculative.
3. **Extend the op set to call-into-interpreter** — JIT'd code
   that hits an unsupported op calls back into the VM with the
   live frame. This is the bridge that lets us ship JIT for
   leaf-arith hot paths without first solving value rep.
4. **NodeId side tables** (per the original phase-1 design). Map
   native PCs back to bytecode PCs / NodeIds so traces and panics
   keep working through JIT'd frames.

Phase 2 (records / closures) still needs the value-rep decision —
the MVP doesn't change that gating.
