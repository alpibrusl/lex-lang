# Issue #129 — Operation log as the store's source of truth

**Status:** design accepted, ready for implementation plan.
**Date:** 2026-05-04.
**Tracker:** [#128 (tier-2 meta)](https://github.com/alpibrupa/lex-lang/issues/128) → [#129](https://github.com/alpibrupa/lex-lang/issues/129).
**Author:** Alfonso Sastre.

## Goal

Promote `Operation` to the unit of writing. The op DAG becomes the durable history; the `SigId → StageId` map every consumer reads is a derived view recomputed from the log on demand. This is the foundation #130 (write-time type-check gate), #131 (intent), #132 (attestations), #133 (predicate branches), and #134 (programmatic merge) all build on.

## Why "replace" rather than "additive"

The first slice of #129 (PR #135) shipped `Operation`, `OperationKind`, `OperationRecord`, and content-addressed `OpId` in `crates/lex-vcs/`. The remaining work has two viable shapes:

- **Additive.** Keep tier-1's `SigId → StageId` branch maps, layer a new `head_op: OpId` field on top, sync the two on every apply.
- **Replacing.** Branches store only `head_op`. The stage map is computed by walking the op DAG.

Lex has no users yet, no on-disk stores to migrate, and no external consumers of the branch JSON shape. Carrying two representations exists only to manage migration cost we don't have. We take the replacing path.

## Scope

In scope for #129:

- New `lex-vcs` modules: `op_log`, `apply`, `diff_to_ops`, `merge`.
- `lex-store` schema change: branches are `{ name, parent, head_op, merges, created_at }`. `head` and `fork_base` are removed.
- `lex-store::apply_operation` glue that drives the apply path and advances the branch head atomically.
- `lex publish` refactor: produces an op sequence rather than writing stages directly.
- `lex blame` refactor: walks the op DAG instead of `lifecycle.json` transitions.
- `lex op show` and `lex op log` subcommands.
- Op-DAG three-way merge replaces the existing `Store::merge` / `commit_merge` engine. The `MergeReport` JSON shape is preserved.
- Conformance descriptors per `OperationKind` and per new subcommand.
- README tier-2 status row.

Explicitly out of scope (deferred to follow-ups):

- `lex op apply <op_id> --to <branch>` (cherry-pick).
- The write-time type-check gate inside `apply_operation` — that is #130. `lex publish` keeps its CLI-side `lex_types::check_program` preview for now.
- Caching `branch_head` results.
- Sharding `<root>/ops/`.
- Multi-writer concurrency / file locking — single-writer assumption holds for the CLI; locking is on the table when `lex serve` becomes a real concurrent producer (likely #130 territory).
- Migration of stores written before this change. Lex has no users; old store directories from prior test runs become unreadable.

## Data model

### `lex-vcs` modules

```
crates/lex-vcs/src/
├── lib.rs              (existing)
├── canonical.rs        (existing)
├── operation.rs        (existing — Operation, OperationKind, OperationRecord, OpId)
├── op_log.rs           (NEW)
├── apply.rs            (NEW)
├── diff_to_ops.rs      (NEW)
└── merge.rs            (NEW)
```

`lex-vcs` gains a `lex-ast` dependency (only `diff_to_ops` needs it; `op_log`, `apply`, and `merge` stay AST-free). `lex-vcs` does *not* depend on `lex-store`; `lex-store` depends on `lex-vcs`.

### `op_log` responsibilities

```rust
pub struct OpLog { /* path: <root>/ops */ }

impl OpLog {
    pub fn open(root: &Path) -> io::Result<Self>;

    /// Persist an OperationRecord. Idempotent: writing an op_id
    /// that already exists is a no-op (same content guaranteed by
    /// content-addressing).
    pub fn put(&self, rec: &OperationRecord) -> io::Result<()>;

    pub fn get(&self, op_id: &OpId) -> io::Result<Option<OperationRecord>>;

    /// Walk parents transitively, newest-first. For a merge op,
    /// both parents' ancestries are traversed (BFS, dedup'd by
    /// op_id). Bounded by `limit` if provided.
    pub fn walk_back(&self, head: &OpId, limit: Option<usize>)
        -> io::Result<Vec<OperationRecord>>;

    /// Same traversal as walk_back, but oldest-first. Used by
    /// `branch_head` so transitions replay left-to-right without
    /// the caller having to reverse the slice.
    pub fn walk_forward(&self, head: &OpId, limit: Option<usize>)
        -> io::Result<Vec<OperationRecord>>;

    /// Lowest common ancestor of two op_ids in the DAG. None if the
    /// two have no shared ancestor (independent histories).
    pub fn lca(&self, a: &OpId, b: &OpId) -> io::Result<Option<OpId>>;

    /// Ops on `head`'s history that are not on `base`'s history.
    /// Used by merge to compute "ops since fork."
    pub fn ops_since(&self, head: &OpId, base: Option<&OpId>)
        -> io::Result<Vec<OperationRecord>>;
}
```

Persistence: one file per op at `<root>/ops/<op_id>.json` (canonical JSON), flat directory. Atomic write via tempfile + rename.

### `apply` responsibilities

```rust
pub fn apply(
    op_log: &OpLog,
    head_op: Option<&OpId>,
    op: Operation,
    transition: StageTransition,
) -> Result<NewHead, ApplyError>;

pub struct NewHead { pub op_id: OpId, pub record: OperationRecord }

pub enum ApplyError {
    StaleParent { expected: Option<OpId>, op_parents: Vec<OpId> },
    Persist(io::Error),
}
```

Behavior:

1. Validate that `op.parents` is consistent with `head_op`:
   - Single-parent op: `op.parents == [head_op]` (or both empty for the genesis op against an empty branch).
   - Merge op: `op.parents.len() == 2` and one parent equals `head_op`. The other must be an ancestor reachable in the log (verified via `op_log.get`).
2. Compute `op_id`.
3. Persist via `op_log.put`. If the op already exists (same `op_id`), return success without rewriting — content-addressing guarantees the bytes match.
4. Return `NewHead`. Caller advances the branch head.

Apply is intentionally narrow for #129: no type checking, no effect verification. #130 layers those into a `verify_operation` call inside this function.

### `diff_to_ops` responsibilities

```rust
pub fn diff_to_ops(
    old_head: &BTreeMap<SigId, StageId>,
    old_effects: &BTreeMap<SigId, EffectSet>,
    old_imports: &BTreeMap<PathBuf, BTreeSet<ModuleRef>>,
    new_stages: &[lex_ast::Stage],
    new_imports: &BTreeMap<PathBuf, BTreeSet<ModuleRef>>,
    diff: &lex_cli::DiffReport,
) -> Vec<OperationKind>;
```

Mapping from `DiffReport` to ops:

| Diff entry | Op kind |
|---|---|
| `added` (fn) | `AddFunction { sig_id, stage_id, effects }` |
| `added` (type) | `AddType { sig_id, stage_id }` |
| `removed` (fn) | `RemoveFunction { sig_id, last_stage_id }` |
| `removed` (type) | `RemoveType { sig_id, last_stage_id }` |
| `renamed` | `RenameSymbol { from, to, body_stage_id }` |
| `modified` (effects changed; body may also have changed) | `ChangeEffectSig { sig_id, from_stage_id, to_stage_id, from_effects, to_effects }` |
| `modified` (body only, type) | `ModifyType { sig_id, from_stage_id, to_stage_id }` |
| `modified` (body only, fn) | `ModifyBody { sig_id, from_stage_id, to_stage_id }` |
| import added | `AddImport { in_file, module }` |
| import removed | `RemoveImport { in_file, module }` |

`ChangeEffectSig` covers the "effects changed and body changed too" case in a single op — its `from_stage_id` / `to_stage_id` span both kinds of change. We do not split into `ChangeEffectSig` + `ModifyBody` for the same fn.

Imports are tracked via the op log itself (option (ii) from design discussion) — there is no `imports/<file>.json` sidecar. To compute `old_imports` for a publish, the caller walks the op log on the current branch and accumulates the per-file import set from `AddImport` / `RemoveImport` ops.

The returned `Vec<OperationKind>` is in dependency order (e.g., `RemoveFunction` for an old name precedes `AddFunction` for a new name, except when the diff identified a rename — in which case a single `RenameSymbol` is emitted).

### `merge` responsibilities

```rust
pub fn merge(
    op_log: &OpLog,
    src_head: Option<&OpId>,
    dst_head: Option<&OpId>,
) -> io::Result<MergeReport>;
```

Algorithm:

1. `lca = op_log.lca(src_head, dst_head)`.
2. `src_ops = op_log.ops_since(src_head, lca.as_ref())`.
3. `dst_ops = op_log.ops_since(dst_head, lca.as_ref())`.
4. Group ops by the `SigId` they touch (`AddImport` / `RemoveImport` group by `(in_file, module)`).
5. For each group:
   - Same `op_id` on both sides → already converged, surface as `merged` with `from = "both"`.
   - Touched on src only → `merged` with `from = "src"`.
   - Touched on dst only → `merged` with `from = "dst"`.
   - Touched on both with different ops → `MergeConflict`. The `kind` string matches the existing tier-1 vocabulary (`modify-modify`, `modify-delete`, `delete-modify`, `add-add`).

`MergeReport` keeps its existing JSON shape (`summary`, `merged`, `conflicts`) so `lex store-merge --json` consumers don't break.

`commit_merge(dst, report)` produces a single merge op with `parents = [src_head, dst_head]`. Two model additions are needed to express it:

- A new `OperationKind::Merge` variant. It does not carry per-sig data on the kind itself — the resolved deltas live in the `produces` field — so its payload is just a marker. Concretely: `Merge { resolved: usize }` (the count is informational, included in the kind so two structurally identical merges of different sizes don't collide on op_id).
- A new `StageTransition::Merge { entries: BTreeMap<SigId, Option<StageId>> }` variant. `Some(stage)` means "after this op, sig points to stage"; `None` means "after this op, sig is removed." The map enumerates only the sigs whose head changed relative to `dst_head` — sigs unaffected by the merge are not listed.

These are both additive to the existing enums. `OperationKind` already uses `#[serde(tag = "op", rename_all = "snake_case")]`, so adding a `Merge` variant does *not* change the canonical bytes of existing variants — `AddFunction` etc. retain their current `op_id`s, and the existing `canonical_form_is_stable_for_a_known_input` golden test stays correct.

### Branch schema

```rust
// crates/lex-store/src/branches.rs (post-change)
pub struct Branch {
    pub name: String,
    pub parent: Option<String>,
    pub head_op: Option<OpId>,        // None = empty branch (no ops yet)
    pub merges: Vec<MergeRecord>,
    pub created_at: u64,
}
```

Removed: `head: BTreeMap<String, String>`, `fork_base: Option<BTreeMap<String, String>>`.

`branch_head(name) -> BTreeMap<SigId, StageId>` becomes a computed function:

```rust
pub fn branch_head(&self, name: &str) -> Result<BTreeMap<SigId, StageId>, StoreError> {
    let b = self.get_branch(name)?.ok_or(StoreError::UnknownBranch(name.into()))?;
    let Some(head_op) = b.head_op else { return Ok(BTreeMap::new()); };
    let mut map = BTreeMap::new();
    for record in self.op_log().walk_forward(&head_op, None)? {
        apply_transition(&mut map, &record.produces);
    }
    Ok(map)
}
```

No caching. `lifecycle.json` is no longer consulted for branch head resolution. It survives as orthogonal stage-status metadata (Draft/Active/Deprecated/Tombstone) and `lex publish --activate` still maintains it.

`main` is a normal branch. Empty stores: `branches/main.json` does not exist; `branch_head("main")` returns an empty map. First publish creates `branches/main.json` with the new `head_op`.

### Storage layout

```
<root>/
├── stages/<SigId>/...              (unchanged)
├── ops/<op_id>.json                (NEW — OperationRecord, canonical JSON)
├── branches/<name>.json            (CHANGED schema)
├── current_branch                  (unchanged)
└── traces/<run_id>/...             (unchanged)
```

`<root>/ops/` flat. Sharding deferred until directory size is a real problem (north of ~10K ops in benchmarks).

## Runtime

### Apply flow

```
lex publish <file>
  └─> parse + canonicalize
  └─> CLI-side preview: lex_types::check_program  (#130 will move this inside apply)
  └─> read current branch (head_op, computed head map, computed import map)
  └─> run ast-diff against head map's stage ASTs
  └─> diff_to_ops -> Vec<OperationKind>
  └─> for each op:
        ├─> persist new stage AST/metadata (existing Store::publish path)
        ├─> Store::apply_operation(branch, op, transition)
        │     └─> lex_vcs::apply(op_log, head_op, op, transition)
        │     └─> rewrite branches/<name>.json with new head_op (atomic)
        └─> append to output
  └─> emit { ops: [...], head_op: ... }
```

### Atomicity

Every persistent write is tempfile-then-rename:

- Op file: write `<root>/ops/<op_id>.json.tmp`, rename to `<op_id>.json`.
- Branch file: write `<root>/branches/<name>.json.tmp`, rename.

We do not currently coordinate "op persisted but branch advance crashed" recovery beyond what filesystem rename atomicity gives us. Crash recovery: if `<op_id>.json` exists but the branch wasn't advanced, the next publish either re-applies the same op (idempotent — same `op_id`, same content) or computes a different op against the un-advanced head and that one supersedes. No data loss; potentially one orphan op. Acceptable for #129; revisit if/when `lex serve` lands.

### Idempotency

- Republishing identical source against the same head produces the same `op_id`s. `OpLog::put` is a no-op for existing op_ids; `apply_operation` is a no-op for an op that's already at the branch head.
- `lex publish` against unchanged source emits zero ops, no error. Required for clean re-runs by agents.

## CLI surface

### New: `lex op show`

```
lex op show <op_id> [--store DIR] [--json]
```

Pretty-print the `OperationRecord`:

```
op_id:     f1129...
kind:      add_function
sig_id:    fac::Int->Int
stage_id:  abc123...
effects:   []
parents:   (none)
produces:  create sig=fac::Int->Int stage=abc123...
```

`--json` emits the canonical `OperationRecord` JSON.

### New: `lex op log`

```
lex op log [--branch <name>] [--limit N] [--store DIR] [--json]
```

Walks the op DAG back from the branch head (current branch by default; unbounded by default). Default text format:

```
op_id    kind             sig_id              note
f11299...  add_function   fac::Int->Int       parents=()
9b8c3d...  modify_body    fac::Int->Int       from=abc123 to=def456
...
```

`--json` emits `[OperationRecord, ...]` newest-first.

### Refactored: `lex publish`

```
lex publish [--store DIR] [--branch NAME] [--activate] [--dry-run] <file>
```

- New flag `--branch` (defaults to current branch).
- Behavior described in "Apply flow" above.
- Output JSON shape:

  ```json
  {
    "ops": [
      { "op_id": "...", "kind": "add_function", "sig_id": "...", "stage_id": "..." }
    ],
    "head_op": "..."
  }
  ```

- No-op publish (zero ops): `{ "ops": [], "head_op": "..." }` and exit 0.
- Type-check failure: existing structured error envelope with exit 2 (unchanged).

### Refactored: `lex blame`

For each fn in the source: walk the op log on the current branch, collect ops touching that `SigId` (or producing it via a `RenameSymbol`), newest-first. Output adds a `causal_history` array of `{ op_id, kind, at, prev_sig_id? }` alongside the existing `history` (which surfaces lifecycle status). The "rename produces one event" acceptance criterion is verified by inspecting the returned `causal_history` array for a renamed fn.

Backwards compatibility on the JSON shape: the existing fields (`name`, `sig_id`, `here_stage_id`, `here_status`, `active_stage_id`, `history`) are preserved. `causal_history` is additive.

### `lex store-merge`

CLI flags and JSON output unchanged. Internally backed by the new op-DAG `merge` engine.

## Conformance

`crates/conformance/` gains:

- One descriptor per `OperationKind` variant (10 variants today + `Merge` added in this PR = 11).
- One descriptor for `lex op show` and one for `lex op log`.
- Descriptor updates for `lex publish` and `lex blame` reflecting the refactored output shapes.

Existing tier-1 conformance for `lex store-merge` is updated to the new merge envelope (the JSON shape is preserved, so most descriptors remain correct; the underlying behavior change for "ops since fork" causes some test-vector updates).

## Tests

Per-module unit tests are obvious; the integration tests that pin the acceptance criteria:

1. `publish` against an empty store creates `branches/main.json` with a `head_op` and writes `<root>/ops/<head_op>.json`.
2. Two consecutive publishes that change the same fn body produce two `ModifyBody` ops with the second's `parents = [first_op_id]`.
3. A rename in the source produces exactly one `RenameSymbol` op (acceptance criterion: `lex blame` shows it as one causal event).
4. Two independent runs publishing the same source against the same parent state produce the same `op_id` (the dedup invariant).
5. Disjoint-sig branches merge clean and produce a `Merge` op with two parents.
6. Same-sig divergent branches produce a `MergeConflict` envelope (`kind: "modify-modify"`).
7. `branch_head` recomputed from the op log equals the SigId→StageId map produced by an equivalent direct stage-write sequence.
8. Republishing identical source emits zero ops; `head_op` is unchanged.

## Acceptance

The following acceptance criteria from #129 are satisfied:

- [x] `lex publish` emits a typed operation sequence; `lex op log` shows it.
- [x] Two independent runs producing the same source against the same parent produce the same `op_id`.
- [x] A rename produces one `RenameSymbol` op, not `RemoveFunction + AddFunction`.
- [x] `lex op show` and `lex op log` documented in the CLI reference table.
- [x] Conformance descriptors covering each operation kind.

## Implementation milestones

The writing-plans phase will turn these into per-step tasks; this is the rough sequence:

1. `lex-vcs::op_log` (persistence + DAG queries, no AST dep). Unit tests in isolation.
2. `lex-vcs::apply` (parent-consistency check, persist op record). Unit tests.
3. Add `OperationKind::Merge { resolved }` and `StageTransition::Merge { entries }` variants. Both are additive — existing variants' canonical bytes are unchanged, so the existing golden hash test stays correct.
4. `lex-store` schema: drop `head`/`fork_base`, add `head_op`. Rewrite `branch_head` as computed view. Update tier-1 tests to the new shape.
5. `lex-store::apply_operation` glue.
6. `lex-vcs::diff_to_ops`. Unit tests per kind.
7. `lex-vcs::merge` (op-DAG three-way). Replaces engine behind `Store::merge` / `commit_merge`. Tier-1 conformance test vectors updated.
8. `lex publish` refactor.
9. `lex blame` refactor.
10. `lex op show` + `lex op log`.
11. Conformance descriptors + README tier-2 status row.
