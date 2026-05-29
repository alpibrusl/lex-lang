# lex-vcs

Agent-native version control for Lex: a typed operation log plus an
attestation graph. Where Git versions *files* by snapshotting text,
lex-vcs versions the *content-addressed AST* by recording typed
deltas — and records the type-check verdict, the prompt that caused
the change, and the evidence that the result is sound, all as
first-class addressable objects.

This is the engine layer. The user-facing surfaces — the CLI
(`lex op`, `lex blame`, `lex attest`, `lex merge`, …) and the HTTP
API (`lex serve` → `/v1/merge/*`, `/v1/stage/<id>/attestations`) —
compose on top of the types this crate exports.

## Why an op log instead of file snapshots

A snapshot-of-pointers model (Git's "named pointer to a tree") can't
answer the questions an agent harness asks:

- "Show me everything agent X did under intent Y in the last hour"
  without that being a pre-named branch.
- "Spawn 20 parallel exploration branches per task and discard 19"
  in `O(1)`.
- "Did this exact verification already run?" without rerunning it.
- "Two agents made the same logical change — do they agree?" without
  a merge.

lex-vcs answers all of these because the unit of writing is an
[`Operation`] — a typed delta keyed by `(kind, payload, parents)` —
and branches are [`Predicate`]s over the log rather than snapshots.

## What's shipped

The crate landed as the foundation slice of #129 and grew through a
sequence of tier-2 issues. Each module is independently usable:

### Core op log (#129)

- **[`Operation`] / [`OperationKind`]** — the typed delta enum:
  `AddFunction`, `RemoveFunction`, `ModifyBody`, `RenameSymbol`,
  `ChangeEffectSig`, `AddImport` / `RemoveImport`, `AddType` /
  `RemoveType` / `ModifyType`, `Merge`, and the #280 typed
  transforms (`InlineLet`, `RenameLocal`, `ReplaceMatchArm`).
  Carries declared `effects` and `[budget(N)]`
  costs inline so the write-time gate and `lex op log
  --budget-drift` don't need to rehydrate the AST.
- **[`OpId`]** — lowercase-hex SHA-256 of the canonical form of
  `(kind, payload, parents)`. Two agents producing the same logical
  change against the same parent state get the same `OpId`, so the
  store dedups and surfaces "we agree" without a merge.
- **[`OpLog`]** — persistence + DAG queries.
  `<root>/ops/<op_id>.json`, atomic tempfile-+-rename writes,
  idempotent on existing ids. Includes packfile consolidation
  (#261): `OpLog::repack` rolls loose files into deterministic,
  content-addressed `pack-<hash>.{pack,idx}` files for stores past
  ~10k ops.
- **[`apply`]** — the narrow apply gate: validate parents against a
  known branch head, then persist. No type checking here (that's the
  next layer).

### Write-time type-check gate (#130)

- **[`check_and_apply`]** wraps `apply` with a `lex_types::
  check_program` pass over the *candidate* program. When this gate is
  the only path that advances a head, the store's invariant becomes
  "every accepted operation produces a program that type-checks" —
  the cascading-breakage failure mode (agent A commits a broken
  stage, agent B builds on it, CI catches it hours later) becomes
  impossible by construction. Effect violations surface here too,
  since `check_program` reports undeclared-effect calls as
  `TypeError`s.

### Intent linkage (#131)

- **[`Intent`] / [`IntentLog`]** — captures *why* a change happened:
  the prompt, the model that interpreted it, and the session that
  grouped it with siblings. [`IntentId`] is the SHA-256 of
  `(prompt, session_id, model, parent_intent)` — `created_at` is
  deliberately excluded so the same prompt dedups across runs.
  Stored in its own namespace (`<root>/intents/<IntentId>.json`) so
  prompts — which may carry sensitive data — stay out of the op log
  and per-intent ACLs remain tractable.

### Attestations (#132)

- **[`Attestation`] / [`AttestationLog`]** — persistent evidence
  about a stage. Where `Operation` records *what* changed and
  `Intent` records *why*, an attestation records *what we know about
  the result*: `TypeCheck`, `Examples`, `Spec`, `DiffBody`,
  `SandboxRun`, `Trace`, plus the governance kinds (`Override`,
  `Defer`, `Block` / `Unblock`, `ProducerBlock` /
  `ProducerUnblock`, `ProducerTrust` / `TrustWaived`) and the
  repair-loop breadcrumbs (`RepairHint`, `RepairAttempt`).
  [`AttestationId`] hashes `(stage_id, op_id, intent_id, kind,
  result, produced_by)` — `cost`, `timestamp`, and `signature` are
  excluded so the same logical verification dedups. Stored at
  `<root>/attestations/<id>.json` with a `by-stage/` index for
  cheap per-stage listing. [`is_stage_blocked`] /
  [`active_producer_block`] drive CI gates.

### Predicate branches (#133)

- **[`Predicate`]** — a saved query over the log
  (`AncestorOf`, `Intent`, `And` / `Or` / `Not`, …). Today's `main`
  is `AncestorOf { op_id: <head> }`; an exploration branch is just
  `Intent { intent_id: <id> }`. Discarding a branch is `O(1)`:
  stop using the predicate; the ops it referenced stay reachable.
  `Author` and `DescendantOf` are deferred pending an `author`
  field and a forward-DAG index (see the module docs).

### Merge (#134)

- **[`merge`]** — op-DAG three-way merge: find the LCA of two heads,
  diff the ops on each side, group by the `SigId` they touch, and
  classify each group as auto-merge or [`ConflictKind`].
- **[`MergeSession`]** — a stateful state machine for programmatic
  conflict resolution: `start` collects conflicts, `resolve` accepts
  batched [`Resolution`]s (re-type-checking each candidate),
  `commit` finalizes once no conflicts remain. The merge cost is
  paid once per session, not once per resolution batch — which
  matches the agent loop "submit 50 resolutions, fix the ones that
  broke type-checking, retry." Backs the CLI `lex merge {start,
  status, resolve, commit}` and the `/v1/merge/*` HTTP routes.

### Diffing (`lex ast-diff` / `lex diff`)

- **[`compute_diff`]** + **[`DiffReport`]** — AST-level structural
  diff between two sets of `FnDecl`s (added / removed / renamed /
  modified, with body patches).
- **[`diff_to_ops`]** — lower a `DiffReport` (plus import deltas and
  old-head info) into the typed [`Operation`] sequence that
  reproduces it. This is how an edited source file becomes a
  committed run of ops.

### Signing (#227)

- **[`Keypair`] / [`verify_stage_id`]** — Ed25519 signing of the
  `StageId` string (not the AST), so authorship survives canonical-
  AST format changes and stays cheap and cross-tool reproducible.
  Public keys → 64 hex chars, signatures → 128 hex chars, lowercase
  no-`0x`, matching the `OpId` / `StageId` convention.

### Format migration (#244)

- **[`migrate`]** — when [`OperationFormat`] gains a variant, every
  `OpId` rotates (the canonical pre-image changes) and parent
  references must be remapped transitively. `plan_migration` returns
  a topologically-ordered [`MigrationPlan`]; `apply_migration` writes
  it in two phases (write-new, then delete-old) so a crash mid-
  migration leaves the store readable. **Today only `OperationFormat::V1`
  is in production** — the machinery exists so the *next* format bump
  is a recipe, not a rewrite. Branch-head and attestation cascades
  are handled by the CLI (`lex store migrate-ops`) and a follow-up,
  respectively (see the module docs).

## Identity and the canonical form

Every id in this crate (`OpId`, `IntentId`, `AttestationId`) is the
lowercase-hex SHA-256 of a **canonical JSON** pre-image. The V1
canonical-form rules are the authoritative spec in
[`src/canonical.rs`] and are load-bearing — violating any of them
silently rewrites every id in every existing store:

1. Compact JSON (no pretty-printing).
2. Field order follows the struct/enum declaration.
3. `BTreeSet` for unordered string sets ([`EffectSet`]).
4. `BTreeMap` for unordered key-value collections
   (`StageTransition::Merge { entries }`).
5. `parents` sorted + deduped before hashing.
6. Empty `parents` arrays *are* emitted in the canonical form (this
   differs from the on-disk shape, which skips them).
7. Optional fields use `skip_serializing_if = "Option::is_none"`, so
   adding a `Some(...)` where `None` was is a deliberate `OpId`
   rotation (the trick that lets `intent_id` / `budget_cost` be
   added without rotating pre-existing ids).
8. SHA-256 → 64 lowercase hex chars.

These are part of the project's stability contract — see
[`docs/INVARIANTS.md`](../../docs/INVARIANTS.md) and the
authoritative spec in
[`docs/design/canonicalization.md`](../../docs/design/canonicalization.md)
before writing code that depends on a hash being stable.

## On-disk layout

```text
<root>/ops/<op_id>.json                          # OperationRecord, loose
<root>/ops/pack-<hash>.{pack,idx}                # consolidated (repack)
<root>/intents/<IntentId>.json
<root>/attestations/<AttestationId>.json         # source of truth
<root>/attestations/by-stage/<StageId>/<id>      # index (rebuildable)
```

All writes are atomic (tempfile + rename) and idempotent on existing
ids.

## Hashing note

This crate uses SHA-256 (via `sha2`) for all identity, reusing what
`lex-store` already uses for stage and signature identity. `OpId` is
opaque, so swapping to Blake3 for performance later is a one-line
crate change.

## Tests

```bash
cargo test -p lex-vcs
```

The suite under `tests/` covers the load-bearing invariants:

- `opid_golden.rs` / `opid_property.rs` — `OpId` stability: golden
  vectors plus property tests that the canonical form is
  order-independent for sets, maps, and parents.
- `attestation_trace.rs` / `attestation_cascade.rs` — attestation
  identity and the block/unblock cascade.
- `budget_accounting.rs` — `[budget(N)]` drift tracking across the
  op kinds that carry `from_budget` / `to_budget`.
- `migrate.rs` — the two-phase format migration and parent remap.
- `repack_conformance.rs` / `merge_perf.rs` — packfile determinism
  and merge performance.

## Where this fits

`lex-vcs` depends only on `lex-ast` and `lex-types` (kept narrow on
purpose — no `lex-store`, no `lex-syntax` in the non-dev path), so it
can be the shared home for `DiffReport` and `diff_to_ops` without a
circular dependency. `lex-store` builds the durable store on top of
it; `lex-cli` and the HTTP API expose it to users.
