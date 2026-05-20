# Lex invariants — an index

Closes [#468](https://github.com/alpibrusl/lex-lang/issues/468).

This doc is an **index**, not a spec. The specs already exist; they're
scattered across `docs/design/canonicalization.md`, a handful of module
headers, and a few `//!` doc-comments. The job of this file is to make
the existing contracts findable in one place, and to be honest about
the parts that are *not yet* contracts.

If you're trying to reason about replay portability, content-addressed
identity, bytecode persistence, or trace stability, start here and
follow the links.

---

## The stability primitives

| Primitive | What it identifies | Hash pre-image | Authoritative source |
|---|---|---|---|
| **SigId** | A canonicalised AST (signature + name + body) | Compact JSON of canonical AST, post-desugar | `crates/lex-ast/src/lib.rs:36-106` + [canonicalization.md §1-3](design/canonicalization.md) |
| **StageId** | A stage (top-level item) of a program | Structural SigId + implementation hash | `crates/lex-ast/src/lib.rs:36-106` |
| **OpId** | A `lex-vcs` operation (the typed-transform VCS unit) | Compact JSON of `Operation`, per 8-rule canonical form | `crates/lex-vcs/src/canonical.rs:1-51` + `crates/lex-vcs/src/operation.rs:494-544` |
| **body_hash** | A compiled function body, **in-process only** | `arity ‖ locals_count ‖ code.len() ‖ serialised ops` | `crates/lex-bytecode/src/program.rs:41-128` + [canonicalization.md §4](design/canonicalization.md) |
| **NodeId** | A node's position within a stage (replay-override key) | Depth-first path string `n_0.<i>.<j>...` | `crates/lex-ast/src/ids.rs:1-37` |
| **EffectSet canonical order** | Effects on a function type | `BTreeSet<EffectKind>` (sorted by `(name, arg)`) | `crates/lex-types/src/types.rs:48-103`, sorting also at `crates/lex-ast/src/canonicalize.rs:99-104` |
| **Canonical AST** | The post-desugar tree that feeds every hash above | Rules in canonicalizer module header | `crates/lex-ast/src/canonicalize.rs:1-12` (module header) + [canonicalization.md §1-3](design/canonicalization.md) |
| **Canonical JSON** | Wire-format used by every hash above | RFC 8785-flavoured: compact, sorted, deterministic | `crates/lex-ast/src/canon_json.rs:1-12` + `crates/lex-vcs/src/canonical.rs:1-51` |

The single most important external doc is
[`docs/design/canonicalization.md`](design/canonicalization.md). It is
the authoritative spec for everything that flows through SigId,
StageId, body_hash, and OpId. **If you're about to write code that
depends on any of those hashes being stable, read it before this one.**

---

## Per-primitive notes

### SigId / StageId

- Derivation: SHA-256 of compact canonical JSON of the canonicalised
  AST. See [canonicalization.md §1-3](design/canonicalization.md).
- **Preserved by:** renaming local bindings, alpha-renaming type
  variables, formatting changes, dead-branch removal (handled by the
  canonicalizer), `if`/`?` → `match` desugaring.
- **Broken by:** any structural source edit that changes the
  canonical-AST shape — adding/removing arms, changing constructor
  names, adding/removing examples, changing the public signature.
- Tests: `crates/lex-ast/tests/canonical.rs:40-146` are the
  golden-master record of these rules.

### OpId

- Derivation: SHA-256 of compact canonical JSON of an `Operation`,
  with the canonical form spelled out in 8 numbered rules at
  `crates/lex-vcs/src/canonical.rs:1-51`. **Read those rules** if
  you're adding fields to `Operation` — they are load-bearing.
- The canonical form is versioned via `OperationFormat::V1`
  (`crates/lex-vcs/src/operation.rs:39-77`), with a forward-migration
  pattern in place. Empty `parents` arrays *are* part of V1 canonical
  bytes even though they're skipped on-disk
  (`canonical.rs` rule 6).

### body_hash

- Derivation: `crates/lex-bytecode/src/program.rs:106-128`
  (`compute_body_hash`). Pre-image is `arity` (u16 LE), `locals_count`
  (u16 LE), `code.len()` (u64 LE), then for each `Op`:
  `byte_length` (u64 LE) followed by `serde_json::to_vec(op)`.
- **Use case:** in-process closure equality only. `Value::Closure`'s
  `PartialEq` compares on `(body_hash, captures)`
  (`crates/lex-bytecode/src/value.rs:118-120`) so two closures
  produced by literally the same source location compare equal even
  when their `fn_id`s differ (#222, `flow.sequential`).
- **Not stable across recompilations.** Constant-pool indices vary
  between compiles (`program.rs:138-142`), so a body_hash written
  to disk and reloaded into a new process is meaningless. See
  [canonicalization.md §4](design/canonicalization.md) for the
  detailed contract. **Do not** persist a body_hash and expect it to
  match later. If you need cross-process closure identity, use SigId
  on the source.

### NodeId

- Definition: `crates/lex-ast/src/ids.rs:8-9` —
  `pub struct NodeId(pub String);`. Format: `n_0` for the stage root,
  with `.i` appended for each child position
  (`n_0.1.2` = "root, second child, third grandchild").
- **Derivation is positional.** The walker (`walk_stage`, `walk_expr`,
  `walk_pat`, `walk_type` in `ids.rs:47-236`) emits NodeIds by
  depth-first traversal order. The post-canonicalization AST is what
  the walker visits.
- **Preserved by:** anything the canonicalizer normalizes away
  (formatting, `if`→`match`, field reordering inside records, etc.) —
  because the post-canonical tree is unchanged.
- **Broken by:** inserting, removing, or reordering AST children
  *that survive canonicalization*. Adding a top-level fn shifts every
  later stage's NodeIds; adding a `let` inside a body shifts every
  later subexpression's NodeIds. There is no current mechanism to
  pin a NodeId across structural edits.
- Used as the key for `lex-trace` replay overrides
  (`crates/lex-trace/src/recorder.rs:30-94`). The replay-stability
  consequence is in the next section.

### Trace replay keys

- Overrides are keyed by NodeId string
  (`crates/lex-trace/src/recorder.rs:93-94`: `overrides: IndexMap<String, serde_json::Value>`).
- **Inherits NodeId stability.** A trace recorded against one source
  state can be replayed against the same source state, or against an
  edit that the canonicalizer normalizes away. Any structural edit
  invalidates the overrides for shifted nodes — they'll either fail
  to find their target or attach to the wrong call.
- The current design is intentional (semantic replay tied to source
  structure, much better than instruction-offset replay) but the
  stability ceiling is "source has not structurally changed since
  recording." Cross-version replay across structural edits is **not**
  supported and is not on the roadmap.

### EffectSet canonical order

- Type: `crates/lex-types/src/types.rs:99-103`.
  `concrete: BTreeSet<EffectKind>` plus optional row variable.
- Iteration is sorted by `EffectKind`'s `Ord` impl
  (`types.rs:48-52` — lexicographic on `(name, arg)`).
- `lex-vcs` carries a simpler `BTreeSet<String>` representation
  (`crates/lex-vcs/src/canonical.rs:20-23`); both canonicalize via
  sorted iteration.

### Canonical AST

- Function: `canonicalize_program` at
  `crates/lex-ast/src/canonicalize.rs:17-25`. Module header
  (`canonicalize.rs:1-12`) lists the rules: alphabetize record-literal
  fields, alphabetize union variants, `if`→`Match`, `?`→`Match`,
  uppercase idents → constructors, pipe desugaring.
- The contract is **the input to every hash above** — when you change
  the canonicalizer, you change every SigId, StageId, and body_hash
  derived from it. [canonicalization.md §6](design/canonicalization.md)
  covers the stability/versioning policy for canonicalizer changes.

### Canonical JSON

- Two implementations, one contract:
  - `crates/lex-ast/src/canon_json.rs:1-12` — RFC 8785-flavoured
    encoding, used for SigId/StageId.
  - `crates/lex-vcs/src/canonical.rs:1-51` — 8-rule spec for
    `Operation`, used for OpId.
- Both produce compact JSON with deterministic field order and
  sorted unordered collections. The `lex-vcs` rules add explicit
  parents-sort-and-dedup and skip-`None` discipline on top.

---

## Gaps — what is *not* yet a contract

These are real today. Calling them out so we don't accidentally rely
on them and so future work has somewhere to land.

### Bytecode has no version tag

`crates/lex-bytecode/src/program.rs` uses auto-derived serde for
`Program` and `Function`. There is no `MAGIC` constant, no
`BYTECODE_REV`, and no compatibility check at deserialize time. The
acknowledgement is at `program.rs:114-128` (the `compute_body_hash`
docstring and the surrounding comment):

> we serialize via `serde_json::to_vec` only because Op's `Serialize`
> impl is auto-derived and stable across Rust versions for this enum
> shape. If determinism ever drifts we'll switch to a hand-rolled
> encoder.

**Current de-facto policy:** bytecode is transient. The CLI compiles
sources at the start of each session and discards the result on exit.
Nothing persists a `Program` to disk and reads it back in a later
process. If/when that changes, this becomes the moment a `MAGIC` +
version pair has to land.

### NodeId stability across structural edits

NodeId is positional (`crates/lex-ast/src/ids.rs:47-236`). There is
no mechanism to *pin* a NodeId across structural edits — inserting a
top-level fn renumbers every stage after it; adding a `let` inside a
body shifts every NodeId after it.

This is fine for the current use case (replay of an unchanged
program) but is the load-bearing limitation for any future use case
that wants to attach metadata to a logical node that survives source
edits. If we need that, the right answer is probably a separate
content-derived identifier (something like a path through the
canonicalized AST keyed by named-symbol breadcrumbs), layered on top
of NodeId rather than replacing it.

### Trace replay across edits

Inherits the NodeId gap. Replay across structural edits is not
supported and is not currently planned.

### body_hash on `Value::Closure` doesn't carry its own warning

[canonicalization.md §4](design/canonicalization.md) covers this in
detail, but the `Value::Closure` struct at
`crates/lex-bytecode/src/value.rs:37-46` doesn't link back to it.
Anyone reading the struct in isolation could assume body_hash is a
durable closure identifier. Worth a doc-comment cross-link in a
future cleanup.

---

## What to read next

- [`docs/design/canonicalization.md`](design/canonicalization.md) —
  the authoritative spec for SigId, StageId, body_hash, OpId.
- [`docs/AGENT_GUIDELINES.md`](AGENT_GUIDELINES.md) — the prescriptive
  rules for writing Lex code (what every agent and human should follow).
- [`crates/lex-vcs/src/canonical.rs:1-51`](../crates/lex-vcs/src/canonical.rs) —
  the 8 rules of OpId canonical form, with the rationale inline.
- [`crates/lex-ast/tests/canonical.rs`](../crates/lex-ast/tests/canonical.rs) —
  the golden-master tests for canonicalization rules; if you're not
  sure whether an edit preserves SigId, the test you'd write to find
  out lives here.
