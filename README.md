# lex-lang

A small family of programming languages designed for LLMs as primary writers, readers, and debuggers. **Lex** is the general-purpose surface; **Core** covers performance-critical work (sized numerics, tensor shapes); **Spec** carries proof annotations.

Implementation of `langspecv2.md`. The design rules and milestone definitions all come from that document; this README focuses on what currently runs.

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

# Type-check a program.
lex check examples/a_factorial.lex

# Run a function with JSON arguments.
lex run examples/a_factorial.lex factorial 5
# → 120

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
lex serve
```

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

### 8. Core — tensor shape solver

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

## Toolchain reference

| Command | Purpose |
|---|---|
| `lex parse <file>` | Print the canonical AST as JSON |
| `lex check <file>` | Type-check; exit 0 or print structured errors |
| `lex hash <file>` | Print SigId / StageId per stage |
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
│   ├── lex-runtime/    # M5: capability policy, effect handlers, pure stdlib builtins
│   ├── lex-store/      # M6: content-addressed store (filesystem)
│   ├── lex-trace/      # M7: trace tree + replay + diff
│   ├── lex-stdlib/     # M11: stdlib stages (in progress; many ops live in lex-runtime)
│   ├── lex-cli/        # M8/M12: command-line tool
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
| M10 — Spec | ✅ randomized + SMT-LIB export ; in-process Z3 deferred |
| M11 — Stdlib MVP | ✅ pure builtins + closures + higher-order list ops + `std.flow` orchestration ; `flow.parallel`/`parallel_record` deferred |
| M16 — Conformance harness + token budget | ✅ |

**Workspace test count:** 125 passing, 0 failing. `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Building from source

Requires a recent Rust toolchain (any 1.80+ stable should work).

```bash
cargo build --release       # full toolchain
cargo test --workspace      # 125 tests
cargo test --release -p core-compiler -- --ignored   # release-only matmul perf gates
```

## License

[EUPL-1.2](LICENSE) — the European Union Public Licence v. 1.2.
