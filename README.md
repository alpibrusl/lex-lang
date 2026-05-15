# lex-lang

[![CI](https://github.com/alpibrusl/lex-lang/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/alpibrusl/lex-lang/actions/workflows/ci.yml)
[![fuzz](https://github.com/alpibrusl/lex-lang/actions/workflows/fuzz.yml/badge.svg?branch=main)](https://github.com/alpibrusl/lex-lang/actions/workflows/fuzz.yml)
[![License: EUPL-1.2](https://img.shields.io/badge/license-EUPL--1.2-blue.svg)](LICENSE)
[![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](#building-from-source)

**The contract layer agents emit into.** Lex is a language plus an
audit substrate purpose-built for the case where an LLM, not a human,
is the primary author of source. Bodies are short-lived; what persists
is the signature, the effect annotation, the content-addressed AST,
and an append-only log of every operation and attestation that
touched the code. Reviewers don't read every line — they read the
contract and trust the substrate to enforce it.

Lex is for the slice where **agents emit source, the runtime decides
whether to run it, and the audit graph survives the next ten model
upgrades.** Other slices (long-running services in Go/Rust, scripts
in Python) compose with Lex rather than replace it — `lex serve` and
`lex tool-registry` give you the embedding seams.

## The agent-code loop

Four moving pieces, end-to-end. Each one runs today; the full
status-by-capability table lives in [§ Status](#status).

### 1. Sandbox — effects-as-types, pre-execution rejection

Every function declares its effects (`[io]`, `[net]`,
`[fs_write("/tmp/...")]`, `[budget(N)]`, `[llm_cloud]`, …). The type
checker refuses any body that reaches outside the declaration *before
a single byte runs*.

```bash
lex agent-tool --allow-effects net --input "x" \
  --body 'match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }'
# → TYPE-CHECK REJECTED — tool not run.
#     effect `io` not declared at n_0
#   exit 2
```

Compare against in-process Python sandboxes — Lex blocks 7/7
adversarial cases pre-execution; RestrictedPython blocks 3/7 and
relies on runtime NameError for the rest. Full report (and the axis
comparison against WASM / Deno / gVisor / Firecracker) in
[`bench/REPORT.md`](bench/REPORT.md).

### 2. Repair — `RepairHint` → `lex repair --apply` → typed transform

When a transform fails type-check, the gate emits a `RepairHint`
attestation carrying the structured errors plus a `suggested_transform`
derived from a static `(rule_tag → typed-transform)` table. The LLM
fix path runs as a typed op, not free-text source rewriting.

```bash
lex --output json repair <failed_op_id> \
  --apply --transform '<transform_json>' --store .lex-store
# → {"outcome":"passed","applied_op_id":"op_..."}
```

Every attempt — success or failure — lands as a `RepairAttempt`
attestation linked to the originating hint, so the chain is
queryable later via `lex blame --with-evidence`.

### 3. Typed-transform VCS — content-addressed stages, structural diff

The store is an append-only log of typed operations (`AddFunction`,
`ModifyBody`, `ReplaceMatchArm`, `Merge`, `Candidate`, `Promote`, …),
each producing a `RepairAttempt` / `TypeCheck` / `Spec` /
`SandboxRun` attestation. Branches are SigId → StageId snapshots
backed by the same log.

```bash
lex publish --activate src/route.lex            # Op: ModifyBody
lex branch create feature
lex store-merge feature main --commit           # 3-way structural merge
lex blame route --with-evidence                 # walk the attestation chain
```

Conflicts surface as JSON, not `<<<<<< HEAD` markers. Body-level
merge is per-stage today; sub-body merge is deferred to tier-3
(see [§ Deferred](#status)).

### 4. Coordination — session budgets, ProducerTrust, multi-agent

Multiple proposers race via `Candidate` / `Promote` without CAS
contention. Per-session budget gates the cost across all participating
agents; `ProducerTrust` scores tools across a rolling window of
attestations.

```bash
# Budget exceeded surfaces as HTTP 503 with structured detail:
curl -X POST localhost:4040/v1/run -d @body.json
# HTTP/1.1 503 Service Unavailable
# Retry-After: 0
# {"error":"session 'sid_xyz' budget exceeded (spent_after=450, cap=400)",
#  "detail":{"kind":"budget_exceeded","cap":400,"spent_after":450}}

# Recompute trust score for a tool against the last 1000 attestations:
lex producer-trust recompute --tool weather-fetcher --window 1000
# → {"tool":"weather-fetcher","ok":true,"attestation_id":"att_..."}

# Cost-aware path planning — cheapest first:
lex plan --goal generate_report --max-cost 500
# plan from `generate_report` (effective cap: 500):
#   session `sid_xyz`: remaining budget 380
#   ok  cost=350 generate_report -> fetch -> summarize
#   no  cost=905 generate_report -> fetch_full -> summarize [io]
```

`Retry-After: 0` on the budget path is the signal: don't retry as-is,
raise the cap or refactor.

## Design rules at a glance

1. **One canonical AST per meaning.** Two programs that mean the same
   thing have the same canonical AST and the same hash. The full
   contract — which edits preserve a SigId and which break it — is in
   [`docs/design/canonicalization.md`](docs/design/canonicalization.md).
2. **Local reasoning.** Any 30-line span is understandable using only
   the types of called functions and stdlib documentation.
3. **Effects are types.** Functions declare their effects in their
   signatures; the compiler enforces them; the runtime sandboxes them.
4. **Errors are values.** No exceptions. `Result[T, E]` is the only
   error channel.
5. **Determinism by default.** Same inputs + same effect responses
   produce the same outputs.
6. **Immutability by default.** Mutation lives in Core, not Lex.
7. **Small total surface.** Grammar fits in ≤ 500 tokens; stdlib index
   ≤ 2000.
8. **The AST is the interface.** Source text is a projection.

## Library landscape

Lex ships a stdlib for the slice it owns — agent-emitted handlers,
tools, and orchestrators.

| Module | Surface | Effect kind |
|---|---|---|
| **`std.http`** | Rich client with builders + decoders; `get` / `post` / `send`; per-host scopes | `[net]` (per-host scoped) |
| **`std.sql`** | Embedded SQLite **or** Postgres (`postgres://...`) under one API; per-connection locking | `[sql, fs_write]` |
| **`std.kv`** | Persistent key-value store via sled | `[kv, fs_write]` |
| **`std.crypto`** | SHA-256/512, BLAKE2b, HMAC, AES-GCM / ChaCha20-Poly1305 AEAD, PBKDF2 / HKDF / Argon2id KDFs, base64 / base64url / hex, constant-time `eq` | pure (KDFs / hashes) ; `[random]` for CSPRNG |
| **`std.conc`** | First-class actors (`conc.spawn` / `conc.ask` / `conc.tell`) — synchronous mailbox model, handler runs on caller's thread | `[concurrent]` |
| **`std.flow`** | `flow.sequential` / `flow.branch` / `flow.parallel_list` orchestration combinators | inherits caller's effects |
| **`std.regex`** | Compiled regex against `re2` syntax | pure |
| **`std.toml`** / **`std.csv`** / **`std.yaml`** | Config / data parsers, polymorphic on the parsed shape | pure |
| **`std.datetime`** | Typed `Instant` + `Duration`; ordering and arithmetic | `[time]` for `now` |
| **`std.list`** / **`std.map`** / **`std.set`** / **`std.option`** / **`std.result`** / **`std.tuple`** / **`std.str`** | Persistent collections, HOFs with effect polymorphism on the closure | pure |

Full per-module signatures live in
[`crates/lex-types/src/builtins.rs`](crates/lex-types/src/builtins.rs).

### Actors and SQL — concrete

```lex
import "std.conc" as conc

fn counter(state :: Int, msg :: Int) -> (Int, Int) {
  let next := state + msg
  (next, next)        # (new state, reply)
}

fn use_counter() -> [concurrent] Int {
  let a := conc.spawn(0, counter)
  let _ := conc.ask(a, 5)     # state becomes 5, reply 5
  conc.ask(a, 3)              # state becomes 8, reply 8
}
```

```lex
import "std.sql" as sql

fn pg_users(conn :: Str) -> [sql, fs_write] Result[List[{ id :: Int, name :: Str }], SqlError] {
  match sql.open(conn) {                  # accepts "postgres://...", ":memory:", or filepath
    Ok(db) => sql.query(db, "SELECT id, name FROM users ORDER BY id", []),
    Err(e) => Err(e),
  }
}
```

## Quickstart

```bash
# Build the toolchain.
cargo build --release
export PATH="$(pwd)/target/release:$PATH"

# Type-check a program. Non-pure programs get an effects hint.
lex check examples/a_factorial.lex
# → ok
lex check examples/c_echo.lex
# → ok
#   required effects: io
#   hint: lex run --allow-effects io examples/c_echo.lex <fn> [args]

# Run a function with JSON arguments.
lex run examples/a_factorial.lex factorial 5
# → 120

# Run a program that uses effects (the runtime refuses without a grant).
lex run --allow-effects io examples/c_echo.lex echo '"hello, lex"'
# → hello, lex

# Capture and inspect a trace.
lex run --trace examples/a_factorial.lex factorial 5
# → trace saved: 6d2e8187...
lex trace 6d2e8187...

# Start the agent API server (HTTP/JSON, port 4040 by default).
lex serve &
curl -sX POST http://localhost:4040/v1/check \
  -H 'content-type: application/json' \
  -d '{"source":"fn add(x :: Int, y :: Int) -> Int { x + y }"}'
# → {"ok": true}
```

### Multi-file projects

Local imports work the same way as `std.*`, just with a path:

```lex
# models.lex
type Status = Healthy | Sick
fn label(s :: Status) -> Str {
  match s { Healthy => "ok", Sick => "nope" }
}

# main.lex
import "./models" as m
fn describe(s :: m.Status) -> Str { m.label(s) }
```

```bash
lex check main.lex   # ok — both files are loaded and merged
```

Path imports are resolved relative to the importer's directory; the
`.lex` extension is auto-appended. `../`, `/abs/path.lex`, and
multi-level nesting all work, with cycle detection.

> Identity is per-file-path today: moving or renaming a `.lex` file
> changes the prefix used in mangled names. The future
> [store-native imports](https://github.com/alpibrusl/lex-lang/issues/82)
> tracker is where content-addressed identity will eventually live.

### LLM-agnostic discovery

Lex implements the [ACLI](https://github.com/alpibrusl/acli) spec, so
**any** LLM agent (Claude Code, Codex, Gemini, Qwen, Mistral, ...) can
discover the surface and call subcommands without a bespoke skill
file:

```bash
lex --output json introspect           # full command tree as JSON
lex skill > LEX.md                     # agentskills.io markdown
lex --output json run app.lex main --dry-run
# {"ok": true, "dry_run": true, "planned_actions": [...]}
```

Every state-modifying command supports `--dry-run` (exit code 9 +
planned-actions envelope); errors come back as ACLI error envelopes
with semantic exit codes. The auto-generated `.cli/` folder is
committed in this repo so agents browsing GitHub can read
`commands.json` without running the binary.

## Toolchain reference

The full table is regenerable via `lex --output json introspect`; the
short list of agent-loop-critical commands:

| Command | Purpose |
|---|---|
| `lex check [--strict] <file>` | Type-check; structured errors carry `file:line:col` + `rule_tag` + `suggested_transform` |
| `lex run [policy] <file> <fn> [args]` | Execute under a capability policy |
| `lex agent-tool --allow-effects k1,k2 --body '<src>'` | Sandbox an LLM-emitted tool body. `--examples` / `--spec` / `--diff-body` cover the correctness ladder |
| `lex tool-registry serve [--port N]` | HTTP service to register Lex tools at runtime |
| `lex publish [--activate] <file>` | Publish stages as typed `Operation`s into the store |
| `lex repair <op_id> --apply --transform <json>` | Apply a typed-transform repair; lands a `RepairAttempt` attestation |
| `lex plan --goal <fn> --max-cost N` | Rank call-graph paths cheapest-first by declared `[budget(N)]` |
| `lex branch create/use/show` + `lex store-merge` | Branch + 3-way structural merge with JSON conflicts |
| `lex blame <fn> --with-evidence` | Per-fn stage history + attestation chain |
| `lex ast-diff` / `lex ast-merge` | AST-native diff and merge; renames register as renames, not delete+add |
| `lex audit --effect K --calls FN --uses-host H` | Structural code search |
| `lex stage promote-candidate <op_id>` | Multi-agent: promote a `Candidate` op to the branch head |
| `lex producer-trust recompute --tool <id>` | Score a tool against its recent attestations |
| `lex spec check <spec> --source <file>` | Property-check a Spec (randomized; SMT-LIB export via `lex spec smt`) |
| `lex serve [--port N]` | Long-running HTTP/JSON API — the embedding seam |
| `lex introspect` / `lex skill` / `lex version` | ACLI discovery |

### Policy flags

```
--allow-effects k1,k2,...   permit these effect kinds (io, net, time, rand, ...)
--allow-fs-read PATH        (repeatable) permit fs_read under PATH
--allow-fs-write PATH       (repeatable) permit fs_write under PATH
--allow-net-host HOST       (repeatable) permit net.* against HOST only
--budget N                  cap aggregate declared budget
```

## Agent API (`lex serve`)

A long-running HTTP/JSON server that exposes the same operations as
the CLI; agents don't pay setup cost per request. Highlights:

| Endpoint | Purpose |
|---|---|
| `POST /v1/check` | `{source}` → `{ok}` or 422 with structured `TypeError` list (each carries `rule_tag` + `suggested_transform`) |
| `POST /v1/publish` | `{source, activate?}` → per-fn `{name, sig_id, stage_id, status}` |
| `POST /v1/run` | `{source, fn, args, policy}` → `{run_id, output}` or 403 / 503 with structured detail |
| `POST /v1/patch` | `{stage_id, patch}` → `{new_stage_id}` (typed transforms applied server-side) |
| `GET  /v1/stage/<id>/attestations` | Walk the attestation chain for a stage |
| `POST /v1/merge/start` / `.../resolve` / `.../commit` | Stateful programmatic merge |

Full list:
[`docs/AGENT.md`](docs/AGENT.md).

## Language tour

If you're new to Lex syntax, this section is the orientation. None of
it is load-bearing for the agent-code loop above — agents that emit
Lex via the API don't need to look at it.

### Factorial — recursion + pattern match

```
fn factorial(n :: Int) -> Int {
  match n {
    0 => 1,
    _ => n * factorial(n - 1),
  }
}
```

### Result, pipes, lambdas

```
import "std.str" as str
import "std.result" as result

type ParseError = Empty | NotNumber

fn parse_int(s :: Str) -> Result[Int, ParseError] {
  if str.is_empty(s) {
    Err(Empty)
  } else {
    match str.to_int(s) {
      Some(n) => Ok(n),
      None    => Err(NotNumber),
    }
  }
}

fn double_input(s :: Str) -> Result[Int, ParseError] {
  parse_int(s) |> result.map(fn (n :: Int) -> Int { n * 2 })
}
```

### Algebraic data types — structural patterns on records

```
type Shape =
    Circle({ radius :: Float })
  | Rect({ width :: Float, height :: Float })

fn area(s :: Shape) -> Float {
  match s {
    Circle({ radius }) => 3.14159 * radius * radius,
    Rect({ width, height }) => width * height,
  }
}
```

### Signature-level examples

A pure function can carry an optional `examples { ... }` block. Each
case is folded into the canonical AST, so the **examples are part of
the signature's identity** — two implementations with different
example sets hash to different SigIds. The type checker validates
every case before a byte runs.

```lex
fn factorial(n :: Int) -> Int
  examples {
    factorial(0) => 1,
    factorial(5) => 120,
  }
{
  match n {
    0 => 1,
    _ => n * factorial(n - 1),
  }
}
```

v1 restricts the block to functions with no declared effects (rule 5
— determinism). See
[issue #369](https://github.com/alpibrusl/lex-lang/issues/369) for the
design and follow-ups.

### Higher-order list ops, closures, orchestration, effects, specs

Runnable example apps live in `examples/`:

| File | Shape |
|---|---|
| `weather_app.lex` | Single-handler REST API with `[net]`-only effects |
| `chat_app.lex` | Multi-user WebSocket chat with `[chat]` effect + room registry |
| `analytics_app.lex` | CSV → group-by → JSON over HTTP, `--allow-fs-read` scope |
| `ml_app.lex` | Linear + logistic regression trained on a 25-row CSV |
| `inbox_app.lex` | Webhook-driven typed-handler router (4 handlers, 4 effect signatures) |
| `gateway_app.lex` | Multi-route service; each route has its own narrow effect set |
| `agent_merge/` | End-to-end multi-agent merge with `Candidate` / `Promote` |

Each header carries an *Adversarial scenario* spelling out what the
runtime gates would reject and the verbatim error string.

### Core — sized numerics + tensor shape solver

The Core sibling adds sized numerics (`U8`–`U64`, `I8`–`I64`,
`F32`/`F64`) and tensors with type-level shape arithmetic. Shape
mismatches are caught at compile time:

```rust
// Calling matmul with mismatched inner dims:
let cs = CoreStage {
    type_params: vec!["M".into(), "N".into()],
    param_types: vec![
        CoreType::Tensor(matrix(var("M"), lit(4), "F64")),
        CoreType::Tensor(matrix(lit(5), var("N"), "F64")),  // 4 ≠ 5
    ],
    return_type: CoreType::Tensor(matrix(var("M"), var("N"), "F64")),
};
// → CoreError::ShapeMismatch { detail: "inner dim 4 ... doesn't match outer dim 5 ..." }
```

A native `matmul` (via `matrixmultiply::dgemm`) is registered in
Core's `NativeRegistry` and callable from Lex.

## For adopters

### Where does Lex source come from?

The default expectation is **agents emit Lex** — through
`lex agent-tool`, `lex tool-registry`, or programmatic calls against
`POST /v1/publish`. Humans typically write the *contract* (function
signature + `examples { ... }` + a `spec` if behaviour matters); the
body is agent-populated and `lex repair --apply`-able.

If you're hand-writing Lex from scratch, that path works too — the
language stands on its own — but you're not in the optimisation
target.

### Embedding

- **`lex serve`** is the long-running HTTP/JSON seam. Owns one
  `Store` instance; agents share it across requests.
- **`lex tool-registry serve`** is the HTTP service for registering
  Lex tools at runtime. `POST /tools` validates + stores;
  `POST /tools/{id}/invoke` runs. Effect manifest is exposed at
  `GET /tools/{id}` so callers can inspect what a tool will reach
  before invoking it.
- **`lex-tea`** (web browser) ships read-only HTML pages over the JSON
  API at the same `lex serve` port: branch list at `/`, stage info +
  attestation trail at `/web/stage/<id>`.

### Editor / IDE

`lex-lsp` is the Language Server. VS Code is supported today via the
generic-LSP extension; setup notes are at
[`crates/lex-lsp/README.md`](crates/lex-lsp/README.md). Features today:
read-only diagnostics, navigation/hover, code-action QuickFix, inline-let
refactor, and the `RepairHint` surface from the store.

### Pre-1.0 stability promise

- **`OpId`s, `SigId`s, attestation hashes** can rotate at any minor
  version pre-1.0. If your tooling keys on them across upgrades, pin a
  Lex version.
- **The Lex source language** (syntax + type rules) is stable within a
  minor; deprecations get one minor of warning before removal.
- **The CLI / HTTP API surface** follows the same minor-cadence rule;
  semantic exit codes don't change.
- **The on-disk Operation log format** is the V1 canonical form
  documented in
  [`crates/lex-vcs/src/canonical.rs`](crates/lex-vcs/src/canonical.rs).
  Formal versioning is tracked by
  [#244](https://github.com/alpibrusl/lex-lang/issues/244); until it
  lands, treat the V1 rules as load-bearing.

## Status

Capability-grouped, ship-readiness-honest. The previous milestone
table is preserved at the bottom of this section for historical
context.

| Capability | Status | Notes |
|---|---|---|
| Effect-typed sandbox + per-path / per-host scopes | **Production-ready** | `lex agent-tool` + `lex run`; 7/7 adversarial blocks in [`bench/REPORT.md`](bench/REPORT.md) |
| Content-addressed AST + typed `Operation` log | **Production-ready** | Tier-2 VCS shipped (#129–#134) |
| Typed transforms (`ReplaceMatchArm`, `RenameLocal`, `InlineLet`, `ExtractFunction`) | **Production-ready** | first-class ops, attested |
| Closed repair loop (`lex repair --apply` + `RepairAttempt`) | **Production-ready** | LLM-driven; every attempt attested |
| Cost-aware planner (`lex plan`) | **Production-ready** | #307 |
| Multi-agent `Candidate` / `Promote` + `ProducerTrust` | **Production-ready** | #293–#294; no CAS contention |
| Per-session budget gate | **Production-ready** | #292 + HTTP 503 `Retry-After: 0` |
| `Stream[T]` + `agent.cloud_stream` | **Production-ready** | chunk-by-chunk LLM consumption |
| `std.conc` actor model | **Production-ready** | #381; synchronous mailbox, mutex-serialised |
| `std.sql` (SQLite + Postgres) | **Production-ready** | `:memory:`, file, or `postgres://...` URI |
| `std.crypto` (AEAD, KDFs, hashes, MAC, encodings) | **Production-ready** | AES-GCM, ChaCha20-Poly1305, Argon2id, PBKDF2, HKDF |
| `std.http` rich client | **Production-ready** | per-host scopes; builders + decoders |
| `lex-lsp` (LSP — VS Code) | **Production-ready** | diagnostics, hover, QuickFix, RepairHint surface |
| `lex-tea` web browser | **Production-ready** (read-only) | branches / fns / stage info |
| Conformance harness + property tests | **Production-ready** | JSON descriptors at `conformance/` |
| Fuzz CI (parser + type-checker) | **Production-ready** | 60s/PR, 5min nightly |
| ACLI compliance (`lex introspect` / `skill` / `version`) | **Production-ready** | every subcommand has `--output text\|json\|table` + `--dry-run` |
| Spec checker (randomized + SMT-LIB export) | **Production-ready** | in-process Z3 deferred |
| `flow.parallel_list` | **Fixture-tested** | sequential v1; true OS-thread parallelism scoped behind `list.par_map` (#305) |
| Core (sized numerics + tensor shapes + native matmul) | **Fixture-tested** | Cranelift JIT + source-level `mut` / `for` syntax deferred |
| `flow.parallel_record` | **Deferred** | needs row polymorphism on records |
| VCS tier-3 (federation, push/pull, body-level merge) | **Deferred** | [#173](https://github.com/alpibrusl/lex-lang/issues/173) |
| JIT (interpreter slice 5) | **Deferred** | [#389](https://github.com/alpibrusl/lex-lang/issues/389) — slices 1–4 shipped |
| In-process Z3 | **Deferred** | spec checker shells out today |
| Store-native imports (content-addressed identity) | **Deferred** | [#82](https://github.com/alpibrusl/lex-lang/issues/82) |

**Workspace test count:** see CI badge above. `cargo clippy --workspace
--all-targets -- -D warnings` is clean. Fuzz CI: 60 s/PR, 5 min
nightly across both targets.

<details>
<summary>Historical milestone view (M0–M16)</summary>

The original milestone-by-milestone status table lived here before the
v1.0 ship-readiness flatten. The capability-grouped view above is
authoritative; the milestone view is preserved in git history at
[main@1ece294](https://github.com/alpibrusl/lex-lang/blob/1ece294/README.md#status).

</details>

## Repository layout

```
lex/
├── crates/
│   ├── lex-syntax/        # Lexer, parser, syntax tree, pretty-printer
│   ├── lex-ast/           # Canonical AST, NodeIds, canonical-JSON, SigId/StageId
│   ├── lex-types/         # HM type checker + effect system
│   ├── lex-bytecode/      # Bytecode definition, compiler, stack VM
│   ├── lex-runtime/       # Capability policy, effect handlers, stdlib builtins
│   ├── lex-store/         # Content-addressed store (filesystem)
│   ├── lex-vcs/           # Typed Operation log + attestations + merge
│   ├── lex-trace/         # Trace tree + replay + diff
│   ├── lex-lsp/           # Language server (VS Code via generic-LSP)
│   ├── lex-search/        # Structural code search (`lex audit` backend)
│   ├── lex-stdlib/        # Reserved for stdlib stages-as-store-entries (pure stdlib lives in lex-runtime)
│   ├── lex-cli/           # CLI; also hosts agent-tool, tool-registry, audit, ast-diff, ast-merge, repair, plan
│   ├── lex-api/           # Agent HTTP/JSON server + lex-tea web UI
│   ├── core-syntax/       # Core lexer/parser (stub; reuses lex-syntax)
│   ├── core-compiler/     # Core type system (shape solver, sized numerics, native matmul)
│   ├── spec-checker/      # Spec proof checker (randomized + SMT-LIB export)
│   └── conformance/       # Conformance harness + property tests
├── examples/              # Lex source examples used by tests and the harness
├── conformance/           # JSON test descriptors (canonical acceptance suite)
├── bench/                 # Adversarial sandbox bench + recordings
└── docs/
    ├── AGENT.md           # Agent API reference
    ├── QUICKSTART.md      # Five-minute tutorial
    └── design/            # Design notes — canonicalization, trace-vs-vcs, etc.
```

## Install

**Pre-built binaries** (no Rust toolchain needed) are attached to
[GitHub Releases](https://github.com/alpibrusl/lex-lang/releases) for
Linux (x86_64 / aarch64), macOS (x86_64 / aarch64), and Windows
(x86_64). Each archive contains the `lex` binary plus README /
LICENSE / CHANGELOG. SHA-256 sums are uploaded alongside each tarball.

```bash
tar -xzf lex-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
mv lex-vX.Y.Z-x86_64-unknown-linux-gnu/lex /usr/local/bin/
lex version
```

**Container image** — `ghcr.io/alpibrusl/lex` is published on every
release tag, multi-arch (`linux/amd64` + `linux/arm64` so it runs
natively on Apple Silicon and Linux arm hosts under Docker / Podman /
containerd):

```bash
# Run the agent VCS server (default; serves on :4040, stores at /data)
docker run -p 4040:4040 -v lex-store:/data ghcr.io/alpibrusl/lex:v0.9.3

# One-shot CLI invocation (subcommand args override the default CMD)
docker run --rm ghcr.io/alpibrusl/lex:v0.9.3 --version
docker run --rm -v "$(pwd):/work" -w /work \
  ghcr.io/alpibrusl/lex:v0.9.3 check src/main.lex

# Use as a base image for a downstream Lex project
# (Dockerfile in your repo)
FROM ghcr.io/alpibrusl/lex:v0.9.3
COPY src /app/src
WORKDIR /app
CMD ["run", "src/main.lex", "main"]
```

The image is drop-in compatible with the local-build `Dockerfile`
this repo ships — same base, same uid 1000 `lex` user, same `/data`
volume, same `lex serve` default — so the Docker Compose stack at
[`docs/deploy.md`](docs/deploy.md) can switch between
`build: .` (fast iteration, cargo-chef cached) and
`image: ghcr.io/alpibrusl/lex:v0.9.3` (fast deploy, ~30s pull vs
~3 min build) without other changes.

## Building from source

Requires a recent Rust toolchain (any 1.80+ stable should work).

```bash
cargo build --release       # full toolchain
cargo test --workspace      # see CI badge for count; --ignored runs slow/flaky examples locally
cargo test --release -p core-compiler -- --ignored   # release-only matmul perf gates

# Optional: run the fuzz suite locally (nightly + cargo-fuzz needed).
cargo install cargo-fuzz --locked
cd fuzz && cargo +nightly fuzz run parser -- -max_total_time=60
```

## License

[EUPL-1.2](LICENSE) — the European Union Public Licence v. 1.2.
