# `lex-trace` ↔ `lex-vcs` integration

**Status:** clarifying doc, no code changes here. Filed in
response to #187 to lock down the layering for downstream
consumers (the `soft` Phase 1 audit-replay deliverable in
particular).

## Summary

- **Traces are not VCS branches.** They live at
  `<store>/traces/<run_id>/trace.json` as standalone artifacts.
  The op log and the attestation graph don't contain them.
- **Run IDs are already content-addressed.** Two processes
  recording independent runs always produce distinct IDs, so
  there is no three-way merge conflict to resolve — only union
  semantics.
- **Cross-store trace sync uses the attestation channel for
  metadata, plus a separate blob copy for the trace JSON.** No
  new merge resolver needed.

If you're building an edge-plus-cloud audit replay (the soft
Phase 1 deliverable), the recipe in [Recommended pattern](#recommended-pattern)
is what to implement; nothing in this doc requires code changes
to `lex-trace` or `lex-vcs` themselves.

## Today

`lex-trace::TraceTree` is one JSON document per run:

```text
<store>/
├── stages/...
├── attestations/...
├── branches/...
└── traces/
    └── <run_id>/
        └── trace.json
```

Persistence APIs:

- `Store::save_trace(&TraceTree) -> Result<run_id>`
- `Store::load_trace(run_id) -> Result<TraceTree>`
- `Store::list_traces() -> Result<Vec<run_id>>`

These are filesystem operations, not VCS operations. They don't
emit `Operation`s, don't advance branch heads, and don't appear
in `lex log`.

`run_id` is `sha256(seed || wall_clock)`. Two runs of the same
program with the same seed in different processes produce
different IDs (because timestamps differ). Same-second collisions
are astronomically unlikely; even if one happens, both files
write to the same path and the second writer wins, which is fine
because the bytes are identical-modulo-timestamp.

## Why traces aren't VCS-tracked

Three forces agree on this:

1. **Traces are a runtime fact, not a definition fact.** The op
   log records "what code exists, who attested to it, who
   activated which stage." A trace records "what happened on a
   specific run." The same body can be exercised by 1000 traces;
   conversely, a trace is meaningful even after the body changes
   (you replay it for audit, not to update HEAD).
2. **Trace JSON is large.** Real-world traces from a multi-hour
   agent run can be tens of MB. Putting them through the op log
   would bloat the log out of proportion to the things it's good
   at indexing (typed operations on stages).
3. **Append-only, no conflicts.** Traces are write-once — the
   recorder finalizes one and never edits it. There's nothing
   for a three-way merge to do.

## Cross-store sync (`store-merge` and friends)

`store-merge` (and the in-flight `MergeSession` engine) walks
operations + attestations only. It does **not** touch the
`traces/` directory. So merging an edge store into a cloud store:

| Artifact | Synced by `store-merge` today? |
|---|---|
| stages, lifecycles | ✅ via op log |
| attestations | ✅ via attestation log |
| branches | ✅ via branch heads |
| traces | ❌ |

That gap is the question soft Phase 1's audit-replay surfaces.
The fix is **not** "teach `store-merge` to merge traces" — it's
"sync the trace blobs alongside the merge."

## Recommended pattern

For an edge-plus-cloud setup that wants to audit-replay later:

### 1. Identify each run with an attestation

When an edge process records a trace, also emit an attestation
that points at the run. The existing
`AttestationKind::SandboxRun { effects }` already serves this
purpose for sandboxed runs. For non-sandboxed runs, either:

- **Option A (recommended for now):** reuse `SandboxRun` with
  the empty effect set when the run wasn't actually sandboxed.
  The attestation is a stand-in for "this trace exists" rather
  than "this run was sandboxed."
- **Option B (cleaner if usage grows):** add a new
  `AttestationKind::Trace { run_id, root_target }` variant so
  the audit channel distinguishes "ran under sandbox" from
  "produced a trace." File a follow-up issue when you have
  enough downstream callers to justify the variant.

The attestation goes through the op log on `store-merge`, so
the cloud store learns about every edge run by virtue of the
existing merge mechanics — no new resolver needed.

### 2. Sync trace blobs out-of-band

`<store>/traces/<run_id>/trace.json` is content-addressed by
`run_id`. Any append-only blob copy works:

- `rsync -av <edge>/traces/ <cloud>/traces/`
- HTTP `PUT /v1/traces/<run_id>` if you want to go through
  `lex-api`
- Object storage (S3 / GCS) as a content-addressed bucket,
  with cloud reads going through a small adapter

**There is no merge to resolve.** Two edges can't both produce
a trace with the same `run_id` because the seed includes the
edge's wall clock (and, in soft's setup, agent identity if you
add it to the seed). If the same `run_id` appears twice the
contents are byte-identical.

If you want stronger collision-resistance (e.g. a deployment
where multiple edges intentionally use synced clocks), seed the
`RunId` with the agent's content-addressed identity in addition
to the wall clock. That work belongs in `lex-trace::RunId::new`
when the use case appears — track as a follow-up.

### 3. Replay from the cloud

Once both the attestation and the blob have arrived, the cloud
process can:

```rust
let tree = cloud_store.load_trace(&run_id)?;
let overrides = collect_overrides_for_audit(&tree);
let result = lex_trace::replay_with_overrides(&program, &overrides)?;
```

The replay itself doesn't care which store the trace came from —
content-addressing makes the trace location-independent.

## What we are explicitly *not* doing

- **No `lex-vcs` op for traces.** Traces stay out of the op log.
  Reverting / branching the op log shouldn't reverberate into
  trace artifacts.
- **No three-way merge of trace contents.** Traces are
  append-only per process and identified by `run_id`. Merging
  two traces from the same run would be a conflict the system
  shouldn't be able to express, much less resolve.
- **No new transport in `lex-trace` for sync.** Blob copy is a
  filesystem / object-storage problem. The trace crate's
  responsibility ends at the JSON document on disk.

## Open follow-ups (file issues if/when you need them)

- `AttestationKind::Trace { run_id, root_target }` variant
  (Option B above), once you want to distinguish sandboxed runs
  from generic trace-produced runs in `lex attest filter`.
- Seed `RunId::new` with agent identity to harden against
  same-clock multi-edge deployments.
- A `lex trace push <remote> --since T` convenience command
  that wraps rsync of `traces/<run_id>/` for the IDs whose
  attestations are in the local op log but not the remote's.
- Per-kind policy on which attestations sync when stores merge
  (the open question from #173). `Trace` would naturally land
  in the "always sync" bucket; `SandboxRun` may want to be
  local-only.

## Acceptance check (per #187)

- [x] Clear answer documented (this file).
- [x] Implementation work scoped — three follow-ups above, none
      blocking soft Phase 1.
- [x] Downstream consumer (soft Phase 1) has a recipe to
      structure its trace-to-VCS pipeline.
