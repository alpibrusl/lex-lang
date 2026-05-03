# lex-lang

[![CI](https://github.com/alpibrusl/lex-lang/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/alpibrusl/lex-lang/actions/workflows/ci.yml)
[![fuzz](https://github.com/alpibrusl/lex-lang/actions/workflows/fuzz.yml/badge.svg?branch=main)](https://github.com/alpibrusl/lex-lang/actions/workflows/fuzz.yml)
[![tests](https://img.shields.io/badge/tests-285_passing-success.svg)](#building-from-source)
[![License: EUPL-1.2](https://img.shields.io/badge/license-EUPL--1.2-blue.svg)](LICENSE)
[![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](#building-from-source)

A language family designed for code no one will read. AI agents write more than humans review; Lex's bet is that when nobody reads bodies, the function signature has to be the contract. Effects are part of the type; the type checker, runtime policy gate, and Spec proofs verify the body honors it — without anyone reading the body.

**Lex** is the general-purpose surface; **Core** covers performance-critical work (sized numerics, tensor shapes); **Spec** carries proof annotations. Implementation of `langspecs.md`; this README focuses on what currently runs.

Full pitch lives at [`docs/index.html`](docs/index.html) (also published via GitHub Pages — see the repo About panel for the live URL).

## Design rules at a glance

1. **One canonical AST per meaning.** Two programs that mean the same thing have the same canonical AST and the same hash.
2. **Local reasoning.** Any 30-line span is understandable using only types of called functions and stdlib documentation.
3. **Effects are types.** Functions declare their effects in their signatures; the compiler enforces them; the runtime sandboxes them.
4. **Errors are values.** No exceptions. `Result[T, E]` is the only error channel.
5. **Determinism by default.** Same inputs + same effect responses produce the same outputs.
6. **Immutability by default.** Mutation lives in Core, not Lex.
7. **Small total surface.** Grammar fits in ≤ 500 tokens; stdlib index ≤ 2000.
8. **The AST is the interface.** Source text is a projection.

## Quickstart

```bash
# Build the toolchain.
cargo build --release

# Add the binary to your path (or invoke ./target/release/lex directly).
export PATH="$(pwd)/target/release:$PATH"

# Type-check a program. Pure programs print `ok`; non-pure ones add
# the effects you'll need to grant when running, plus a suggested
# `lex run` command.
lex check examples/a_factorial.lex
# → ok
lex check examples/c_echo.lex
# → ok
#   required effects: io
#   hint: lex run --allow-effects io examples/c_echo.lex <fn> [args]

# Run a function with JSON arguments.
lex run examples/a_factorial.lex factorial 5
# → 120

# Variants are passed as `{"$variant": "Name", "args": [...]}` —
# the same shape the runtime emits on output, so `lex run` results
# can be piped back as inputs to other calls.
lex run examples/b_parse_int.lex double_input '"21"'
# → {"$variant":"Ok","args":[42]}

# Run a program that uses effects (the runtime refuses without a grant).
lex run examples/c_echo.lex echo '"hello, lex"'
# → {"kind":"effect_not_allowed", ...}; exit 3
lex run --allow-effects io examples/c_echo.lex echo '"hello, lex"'
# → hello, lex

# Capture a trace to disk and inspect / replay it.
lex run --trace examples/a_factorial.lex factorial 5
# → trace saved: 6d2e8187...
lex trace 6d2e8187...

# Publish stages to the content-addressed store, list them, fetch one.
lex publish --activate examples/a_factorial.lex
lex store list
lex store get <stage_id>

# Run the conformance harness against the canonical descriptors.
lex conformance conformance/

# Start the agent API server (HTTP/JSON, port 4040 by default).
lex serve &

# In another shell — type-check a snippet over HTTP:
curl -sX POST http://localhost:4040/v1/check \
  -H 'content-type: application/json' \
  -d '{"source":"fn add(x :: Int, y :: Int) -> Int { x + y }"}'
# → {"ok": true}

# Run a function over HTTP with policy:
curl -sX POST http://localhost:4040/v1/run \
  -H 'content-type: application/json' \
  -d '{"source":"fn add(x :: Int, y :: Int) -> Int { x + y }",
       "fn":"add", "args":[2,3], "policy":{"allow_effects":[]}}'
# → {"run_id":"...","output":5}
```

### Multi-file projects

Local imports work the same way as `std.*`, just with a path:

```bash
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
lex run main.lex describe '{"$variant":"Healthy","args":[]}'
```

Path imports are resolved relative to the importer's directory; the
`.lex` extension is auto-appended. `../`, `/abs/path.lex`, and
multi-level nesting all work, with cycle detection that reports the
full path chain. Stdlib imports are unchanged.

The natural `types/ + behavior/ + runner/` layout works too: when
two siblings each `import "./models" as m`, both reach the same
`Report` nominal type — the loader keys mangling on the canonical
filesystem path and dedupes second loads, so diamond-shaped imports
collapse to one identity.

> Identity is per-file-path today: moving or renaming a `.lex` file
> changes the prefix used in mangled names. The future
> [store-native imports](https://github.com/alpibrusl/lex-lang/issues/82)
> tracker is where content-addressed identity will eventually live.

### Quickstart: agent-native tooling

```bash
# Sandbox an LLM-emitted tool body. Effects outside --allow-effects are
# rejected at type-check, before any code runs.
lex agent-tool --allow-effects net --input "x" \
  --body 'match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }'
# → TYPE-CHECK REJECTED — tool not run.   exit 2

# Audit a codebase by structure: every fn that touches the network.
lex audit --effect net examples/

# AST-native diff. Renames register as "renamed", not "delete + add".
lex ast-diff before.lex after.lex

# Three-way structural merge. Conflicts are JSON, not <<<<<< HEAD markers.
lex ast-merge base.lex ours.lex theirs.lex

# Snapshot branches in the store. SigId → StageId map per branch;
# three-way merge with structured JSON conflicts.
lex branch create feature
lex branch use feature
# … publish edits to the feature branch …
lex store-merge feature main          # preview the merge
lex store-merge feature main --commit # apply when clean

# Runtime tool registry: register Lex tools over HTTP, get back a stable
# /tools/{id}/invoke endpoint with the effect manifest at /tools/{id}.
lex tool-registry serve --port 8390
```

### LLM-agnostic discovery

Lex implements the [ACLI](https://github.com/alpibrusl/acli) spec, so
**any** LLM agent (Claude Code, Codex, Gemini, Qwen, Mistral, ...)
can discover the surface and call subcommands without a bespoke skill
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

## Examples

### 1. Factorial — recursion + pattern match

```
fn factorial(n :: Int) -> Int {
  match n {
    0 => 1,
    _ => n * factorial(n - 1),
  }
}
```

```bash
lex run examples/a_factorial.lex factorial 10
# → 3628800
```

### 2. Parse and double — Result, pipes, lambdas

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

```bash
lex run examples/b_parse_int.lex double_input '"21"'
# → {"$variant":"Ok","args":[42]}

lex run examples/b_parse_int.lex double_input '""'
# → {"$variant":"Err","args":[{"$variant":"Empty","args":[]}]}
```

### 3. Algebraic data types — structural patterns on records

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

A bare record pattern matches a nominal record alias too — useful
for flat decision tables over a fixed set of fields:

```
type Bands = { idea :: Str, execution :: Str }

fn verdict(b :: Bands) -> Str {
  match b {
    { idea: "high", execution: "high" } => "ship",
    { idea: "high", execution: _      } => "iterate",
    _                                   => "park",
  }
}
```

### 4. Higher-order list ops — closures over outer state

```
import "std.list" as list

fn sum_even_squares(xs :: List[Int]) -> Int {
  let evens   := list.filter(xs, fn (n :: Int) -> Bool { (n % 2) == 0 })
  let squared := list.map(evens, fn (n :: Int) -> Int { n * n })
  list.fold(squared, 0, fn (acc :: Int, x :: Int) -> Int { acc + x })
}
```

`sum_even_squares([1, 2, 3, 4, 5, 6])` returns 56.

### 5. Orchestration — `flow.branch` over `flow.sequential`

```
import "std.flow" as flow

fn abs_double() -> (Int) -> Int {
  flow.branch(
    fn (n :: Int) -> Bool { n >= 0 },
    flow.sequential(fn (n :: Int) -> Int { n },     fn (n :: Int) -> Int { n * 2 }),
    flow.sequential(fn (n :: Int) -> Int { 0 - n }, fn (n :: Int) -> Int { n * 2 })
  )
}
```

The returned closure is itself a Lex value; pass it around or bind it to a stage and call later.

### 6. Effects — `io.print` gated by capability policy

```
import "std.io" as io

fn echo(line :: Str) -> [io] Nil {
  io.print(line)
}
```

```bash
# Refused at the policy gate, before any code runs:
lex run examples/c_echo.lex echo '"x"'
# {"kind":"effect_not_allowed","detail":"effect `io` not in --allow-effects",
#  "effect":"io","at":"echo"}
# exit 3

# With the grant:
lex run --allow-effects io examples/c_echo.lex echo '"x"'
# x
```

### 7. Specs — randomized property checking + SMT-LIB export

```
spec clamp {
  forall x :: Int, lo :: Int, hi :: Int where lo <= hi:
    let r := clamp(x, lo, hi)
    (r >= lo) and (r <= hi)
}
```

```bash
lex spec check clamp.spec --source clamp.lex --trials 1000
# {"spec_id":"...","status":"proved",
#  "evidence":{"method":"randomized","trials":1000,...}}

lex spec smt clamp.spec
# (SMT-LIB 2 script for `z3 -smt2 -`)
```

### 8. Real-world examples

Eight runnable example apps live in `examples/`:

| File | Shape |
|---|---|
| `weather_app.lex` | Single-handler REST API with [net]-only effects |
| `chat_app.lex` | Multi-user WebSocket chat with [chat] effect + room registry |
| `analytics_app.lex` | CSV → group-by → JSON over HTTP, with `--allow-fs-read` scope |
| `ml_app.lex` | Linear + logistic regression trained on a 25-row CSV; `/predict_*` endpoints |
| `inbox_app.lex` | Webhook-driven typed-handler router (4 handlers, 4 effect signatures) |
| `gateway_app.lex` | Multi-route service; each route has its own narrow effect set |
| `agent_tool` (binary) | LLM-emitted tool sandbox — see `Sandboxing agent-generated code` below |
| `tool-registry` (binary) | HTTP service for runtime tool registration with effect manifests |

Each example header contains an *Adversarial scenario* spelling out what the runtime gates would reject and the verbatim error string.

### 9. Core — tensor shape solver

The Core sibling adds sized numerics (`U8`–`U64`, `I8`–`I64`, `F32`/`F64`) and tensors with type-level shape arithmetic. Shape mismatches are caught at compile time:

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

A native `matmul` (via `matrixmultiply::dgemm`) is registered in Core's `NativeRegistry` and callable from Lex.

## Sandboxing agent-generated code

`lex agent-tool` asks Claude for a tool body, splices it into a fixed signature, and runs it under a declared effect set. The type checker rejects any body that reaches outside that set, **before a single byte runs**.

```bash
lex agent-tool --allow-effects net \
  --request 'fetch http://example.com and return its length'
# → ok len=1256

lex agent-tool --allow-effects net --input "x" \
  --body 'match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }'
# → TYPE-CHECK REJECTED — tool not run.
#     effect `io` not declared at n_0
#   exit 2
```

Flags: `--body '<src>'` / `--body-file <path>` skip the API call; `--request '<q>'` needs `ANTHROPIC_API_KEY`. `--max-steps N` (default 1M) caps op count as a runtime DoS guard. `--allow-fs-read PATH` and `--allow-net-host HOST` add per-path / per-host scopes on top of `--allow-effects`. `--examples FILE` runs the tool against a JSON list of `{input, expected}` pairs (exit 5 on mismatch). `--spec FILE` proves a behavioral contract against the emitted body via the spec checker (exit 5 on counterexample, 6 on inconclusive). `--diff-body 'src'` / `--diff-body-file FILE` runs a second body on the same inputs and exits 7 on output divergence — regression detection across model upgrades.

### Adversarial benchmark

|  | Actively blocked | Benign allowed | Mechanism |
|---|---|---|---|
| **Lex** | **7 / 7** | 2 / 2 | static effect typing — pre-execution |
| Python (naive `exec`) | 0 / 7 | 2 / 2 | `__builtins__` allowlist + string blocklist |
| Python (RestrictedPython) | 3 / 7 | 2 / 2 | AST rewrite + `safe_builtins` + `safer_getattr` |

7 attacks + 2 benign cases through three sandboxes. Full report at [`bench/REPORT.md`](bench/REPORT.md); regenerate with `cargo test -p lex-cli --test agent_sandbox_bench`. The structural pitch — *opt-in granting from a sandboxed default* vs *opt-in restriction of an unrestricted base*, type-check rejection vs runtime NameError — is on the [project landing page](docs/index.html).

### Live demos

Two asciinema scripts ship under `bench/`:

- **[`bench/RECORDING.md`](bench/RECORDING.md)** — `lex agent-tool` blocking
  Claude-emitted code that tries to escape the declared effect set.
- **[`bench/RECORDING_VC.md`](bench/RECORDING_VC.md)** — agent-native VC
  walkthrough: ACLI discovery, structural diff with effect highlighting,
  `lex branch` + `lex store-merge` with a JSON conflict, `lex log`,
  `lex blame`. The workflow companion to the security demo.

Recorded `.cast` files live under `bench/` once captured; `agg` converts
them to GIFs for README / Twitter / LinkedIn.

## Toolchain reference

| Command | Purpose |
|---|---|
| `lex parse <file>` | Print the canonical AST as JSON |
| `lex check <file>` | Type-check; exit 0 or print structured errors |
| `lex repl` | Interactive evaluator. `fn`/`type`/`import` extend the session; expressions are evaluated under a permissive policy. `.help`, `.list`, `.reset`, `.quit` |
| `lex watch <file> [check\|run] [args...]` | Re-run on every save. Default action is `check`; `run` re-executes. Forwarded args (`--allow-effects ...`) pass through to the underlying subcommand |
| `lex hash <file>` | Print SigId / StageId per stage |
| `lex blame [--store DIR] <file>` | Per-fn stage history from the store: which StageId is currently in source, which is Active, predecessors with statuses + timestamps |
| `lex run [policy] <file> <fn> [args]` | Execute a function (args are JSON) |
| `lex publish [--store DIR] [--activate] <file>` | Publish stages to the content-addressed store |
| `lex store list` / `lex store get <id>` | Browse the store |
| `lex run --trace ...` | Save a trace tree under the store |
| `lex trace <run_id>` | Print a saved trace tree |
| `lex replay <run_id> <file> <fn> [args] [--override NODE=JSON]` | Re-execute with effect overrides |
| `lex diff <run_a> <run_b>` | First NodeId where two traces diverge |
| `lex conformance <dir>` | Run JSON test descriptors |
| `lex spec check <spec> --source <file> [--trials N]` | Property-check a Spec |
| `lex spec smt <spec>` | Emit SMT-LIB 2 for external Z3 |
| `lex serve [--port N] [--store DIR]` | Run the agent HTTP/JSON API |
| `lex agent-tool --allow-effects ks (--request 'q' \| --body 'src' \| --body-file F)` | Run an LLM-emitted tool body under declared effects |
| `lex tool-registry serve [--port N]` | HTTP service to register Lex tools at runtime; `POST /tools` validates + stores, `POST /tools/{id}/invoke` runs |
| `lex audit [paths...] [--effect K] [--calls FN] [--uses-host H] [--kind K]` | Structural code search by effect / call / hostname / AST kind. `--json` for agent-pipe output |
| `lex ast-diff <file_a> <file_b> [--json] [--no-body]` | AST-native diff: added / removed / renamed / modified fns, plus body-level patches. Renames detected by body-hash with name normalized |
| `lex ast-merge <base> <ours> <theirs> [--json] [--write PATH] [--dry-run]` | Three-way structural merge. Conflicts surface as JSON (4 kinds: modify-modify, modify-delete, delete-modify, add-add). Exit 2 on any conflict; `--write` materializes merged source when clean |
| `lex branch <list \| show \| create \| delete \| use \| current> [--store DIR]` | Snapshot branches in the store. Each branch is a SigId → StageId map persisted at `<store>/branches/<name>.json`. Default `main` materializes from the existing lifecycle |
| `lex store-merge <src> <dst> [--commit] [--json]` | Three-way merge between two branches; common ancestor is the source branch's `fork_base` snapshot, not the parent's current head. Conflict kinds match `ast-merge`. `--commit` applies a clean merge to dst |
| `lex introspect [--output text\|json\|table]` | Full command tree per ACLI §1.2 (name, description, args, options, idempotency, examples, see-also). Auto-generated `.cli/commands.json` is also committed in the repo |
| `lex skill` | agentskills.io-compliant Markdown for Claude Code / Codex / Gemini skill directories |
| `lex version [--output json]` | Tool version + ACLI spec version, JSON-enveloped under `--output json` |

### Policy flags (run / replay)

```
--allow-effects k1,k2,...   permit these effect kinds (io, net, time, rand, ...)
--allow-fs-read PATH        (repeatable) permit fs_read under PATH
--allow-fs-write PATH       (repeatable) permit fs_write under PATH
--budget N                  cap aggregate declared budget
```

## Agent API (`lex serve`)

A long-running HTTP/JSON server that exposes the same operations as the CLI. The server owns a `Store` instance, so agents don't pay setup cost per request.

| Endpoint | Purpose |
|---|---|
| `GET  /v1/health` | `{ok: true}` |
| `POST /v1/parse` | `{source}` → CanonicalAst \| 4xx |
| `POST /v1/check` | `{source}` → `{ok}` \| 422 with structured TypeError list |
| `POST /v1/publish` | `{source, activate?}` → `[{name, sig_id, stage_id, status}, ...]` |
| `POST /v1/patch` | `{stage_id, patch}` → `{new_stage_id}` \| 422 with structured TypeError list if the patched AST doesn't type-check |
| `GET  /v1/stage/<id>` | `{metadata, ast, status}` |
| `POST /v1/run` | `{source, fn, args, policy}` → `{run_id, output \| error}`; 403 with structured policy violation if disallowed |
| `GET  /v1/trace/<run_id>` | TraceTree |
| `POST /v1/replay` | `{source, fn, args, policy, overrides}` → `{run_id, output \| error}` |
| `GET  /v1/diff?a=&b=` | `Divergence` \| `{divergence: null}` |

All structured errors come back as `{error, detail?}` with details parseable by callers.

## Repository layout

```
lex/
├── crates/
│   ├── lex-syntax/     # M1: lexer, parser, syntax tree, pretty-printer
│   ├── lex-ast/        # M2: canonical AST, NodeIds, canonical-JSON, SigId/StageId
│   ├── lex-types/      # M3: HM type checker + effect system
│   ├── lex-bytecode/   # M4: bytecode definition, compiler, stack VM
│   ├── lex-runtime/    # M5: capability policy, effect handlers, all stdlib builtins live here
│   ├── lex-store/      # M6: content-addressed store (filesystem)
│   ├── lex-trace/      # M7: trace tree + replay + diff
│   ├── lex-stdlib/     # M11: reserved for stdlib stages-as-store-entries (currently a stub;
│   │                   #      pure stdlib lives in lex-runtime/builtins.rs)
│   ├── lex-cli/        # M8/M12: command-line tool (also hosts agent-tool, tool-registry,
│   │                   #         audit, ast-diff, ast-merge subcommands)
│   ├── lex-api/        # M8/M12: agent HTTP/JSON server
│   ├── core-syntax/    # M13: Core lexer/parser (stub; reuses lex-syntax)
│   ├── core-compiler/  # M9:  Core type system (shape solver, sized numerics, mut analysis, native matmul)
│   ├── spec-checker/   # M10: Spec proof checker (randomized + SMT-LIB export)
│   └── conformance/    # M16: conformance harness + property tests
├── examples/           # Lex source examples used by tests and the harness
├── conformance/        # JSON test descriptors (canonical acceptance suite)
└── docs/
```

## Status

| Milestone | Status |
|---|---|
| M0 — Skeleton | ✅ |
| M1 — Lexer + parser | ✅ |
| M2 — Canonical AST + NodeIds | ✅ |
| M3 — Type checker (HM + effects) | ✅ |
| M4 — Bytecode + VM | ✅ |
| M5 — Effect runtime + capability layer | ✅ |
| M6 — Content-addressed store | ✅ |
| M7 — Trace tree + replay + diff | ✅ |
| M8 — CLI + agent API server | ✅ |
| M9 — Core | Phase 1 (shape solver, sized numerics) ✅ ; Phase 2 (mutation analysis, native matmul) ✅ ; Cranelift JIT, source-level `mut`/`for` syntax deferred |
| M10 — Spec | ✅ randomized + SMT-LIB export ; `--spec` wired into `agent-tool` ; in-process Z3 deferred |
| Stdlib MVP | ✅ pure builtins + closures + higher-order list ops + `std.flow` orchestration ; `std.math` (linalg + scalar floats) ; `std.tuple` ; **effect polymorphism** on `list.map` / `list.filter` / `list.fold` / `option.map` / `result.map` / `result.and_then` / `result.map_err` ; `flow.parallel` ✅ (sequential v1 — true threading deferred) ; **`std.map` ✅ ; `std.set` ✅** (persistent collections with `Str`/`Int` keys) ; `flow.parallel_record` deferred (needs row polymorphism on records) |
| Conformance harness + token budget | ✅ |
| Agent integration (post-spec) | `lex agent-tool` (sandbox) ✅ ; `lex tool-registry serve` (HTTP registry) ✅ ; correctness ladder: `--examples` ✅ `--spec` ✅ `--diff-body` ✅ ; AST tooling: `lex audit` ✅ `lex ast-diff` (with effect-change highlighting) ✅ `lex ast-merge` ✅ ; **`lex blame` ✅** (per-fn stage history from the store) |
| Agent-native version control | tier-1 ✅ — `lex branch` + `lex store-merge` with `fork_base` snapshots and structured JSON conflicts ; **`lex log` ✅** (per-branch merge journal) ; distributed sync + body-level merge deferred |
| LLM-agnostic discovery | ✅ — full [ACLI](https://github.com/alpibrusl/acli) compliance: `lex introspect` / `lex skill` / `lex version`, `--output text\|json\|table` on every subcommand, `--dry-run` on state-modifying ones, error envelopes with semantic exit codes |
| Hardening | [`SECURITY.md`](SECURITY.md) threat model ✅ ; parser-recursion DoS gate (`MAX_DEPTH=96`) ✅ ; **VM call-stack depth gate (`MAX_CALL_DEPTH=1024`) ✅** ; libFuzzer CI for parser + type checker ✅ ; VM-level memory bounds remain delegated to the host (container memory caps) |

**Workspace test count:** 285 passing, 0 failing, 3 ignored (WS chat example, flaky on CI runners — pass locally with `--ignored`). `cargo clippy --workspace --all-targets -- -D warnings` clean. Fuzz CI: 60 s/PR, 5 min nightly across both targets.

## Install

**Pre-built binaries** (no Rust toolchain needed) are attached to
[GitHub Releases](https://github.com/alpibrusl/lex-lang/releases)
for Linux (x86_64 / aarch64), macOS (x86_64 / aarch64), and
Windows (x86_64). Each archive contains the `lex` binary plus
README / LICENSE / CHANGELOG. SHA-256 sums are uploaded alongside
each tarball.

```bash
# Pick the right archive for your platform from the Releases page,
# then:
tar -xzf lex-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
mv lex-vX.Y.Z-x86_64-unknown-linux-gnu/lex /usr/local/bin/
lex version
```

## Building from source

Requires a recent Rust toolchain (any 1.80+ stable should work).

```bash
cargo build --release       # full toolchain
cargo test --workspace      # 285 tests (+ 3 ws_chat ignored — `--ignored` to run locally)
cargo test --release -p core-compiler -- --ignored   # release-only matmul perf gates

# Optional: run the fuzz suite locally (nightly + cargo-fuzz needed).
cargo install cargo-fuzz --locked
cd fuzz && cargo +nightly fuzz run parser -- -max_total_time=60
```

## License

[EUPL-1.2](LICENSE) — the European Union Public Licence v. 1.2.
