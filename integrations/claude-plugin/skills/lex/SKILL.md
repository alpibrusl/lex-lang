---
name: lex
description: Write, type-check, repair, publish, and run Lex (lex-lang) code via the `lex` CLI. Use whenever working with `.lex` files, the Lex typed-effect language, the content-addressed AST / attestation store, or a lex-hub gateway. Covers the agent-code loop (check → repair → publish → run), the effect-discipline idiom rules, and the syntax pitfalls (`::`, `:=`, `->`).
paths:
  - "**/*.lex"
  - "**/lex.toml"
---

# Working with Lex

Lex is a typed-effect language with a content-addressed AST and an
attestation graph, purpose-built for code an LLM (you) authors. Every
function declares its **effects** (`[io]`, `[net]`, `[fs_write("/tmp/x")]`,
…); the type checker rejects any body that reaches outside its declaration
*before a byte runs*. Source is a projection of the canonical AST — what
persists is the signature, the effects, and the attestation chain.

You drive Lex through the **`lex` CLI**, which is self-describing (it
implements the ACLI discovery contract). For a **remote/hosted** store, use
the `lex-hub` MCP tools instead (see the bottom of this file).

## First: confirm the toolchain and read the contract

```bash
lex --version                       # confirm `lex` is installed
lex agent-guidelines                # AUTHORITATIVE idiom rules — read in full
lex --output json introspect        # full command tree as JSON
lex skill                           # CLI surface + semantic exit codes
```

`lex agent-guidelines` is the source of truth for project conventions. If
`lex` is not installed, install a release from
<https://github.com/alpibrusl/lex-lang/releases> (or `cargo build --release`
in a checkout) before continuing. Pin the version the project's `lex.toml` /
CI declares.

## The loop

Every change goes through the same steps. **Do not claim a task done until
they're all green.**

```bash
lex check --strict <file>           # type-check (+ extra lints)
lex fmt --check <file>              # canonical formatting
lex test                            # runs tests/test_*.lex
lex ci                              # umbrella: check + fmt + test + pkg install
```

To run a single function (args are JSON):

```bash
lex check <file>                                   # always check first
lex run <file> <fn> '"arg"' 42                     # pure fn
lex run --allow-effects io <file> echo '"hi"'      # grant effects explicitly
```

## The ten rules that matter most

These distill `lex agent-guidelines`; read the full doc for the rest.

1. **Narrow effects, always (MUST).** Declare the *narrowest* effect set,
   with path/host scopes: `[fs_write("/var/log/app.log")]`, not
   `[fs_write]`, never `[io, fs_read, fs_write, net]`. Wide annotations
   typecheck but defeat the entire point of the sandbox.

2. **Never broaden effects to satisfy the checker (MUST).** When `lex check`
   says "effect `fs_write` not declared at line X", the fix is almost never
   "add `fs_write` to the signature" — it's "remove/narrow the code that
   reached for it." If a fn should be pure and isn't, **the body is wrong**.

3. **Repair, don't regenerate (MUST → SHOULD).** On a check failure, get the
   structured error with `lex --output json check`, then apply its
   `suggested_transform` via `lex repair --apply` rather than rewriting the
   body. Only regenerate after **two** failed repair attempts. See
   `/lex:lex-repair`.

4. **`examples {}` on every pure fn (SHOULD).** They fold into the canonical
   AST (part of the SigId) and run at `lex check` time — free regression
   tests:
   ```lex
   fn add(x :: Int, y :: Int) -> Int
     examples { add(2, 3) => 5, add(-1, 1) => 0 }
   { x + y }
   ```

5. **`Result[T, E]` / `Option[T]`, no exceptions, no null (MUST).** Match or
   pipe through `result.map` / `result.and_then` / `result.map_err`.

6. **Exhaustive matches (AVOID stray `_ =>`).** List every variant; reserve
   `_ =>` for genuine catch-alls. A silent `_` swallows variants added later.

7. **Use the stdlib (MUST).** `std.crypto` not hand-rolled crypto, `std.sql`
   with parameterised queries not string-concat, `std.conc` actors not OS
   threads, `std.http` with per-host scopes not raw sockets, `std.regex` not
   manual scanners. Check the stdlib index before reaching for raw bytes.

8. **Don't churn the SigId (AVOID).** Cosmetic edits cost real money — they
   invalidate attestations. Don't rename params/locals, reorder top-level
   fns, or hand-reformat. Run `lex fmt` (it's canonical) and leave it.

9. **Respect the budget gate (MUST).** HTTP `503` + `Retry-After: 0` from a
   run means "don't retry as-is; raise the cap or refactor to spend less."
   Auto-retrying makes the cap meaningless.

10. **Query before you redo work.** `lex blame <fn> --with-evidence` shows
    the TypeCheck / Spec / Examples / SandboxRun / RepairAttempt chain that
    already covers a fn. Don't re-attest what the substrate has.

## Syntax pitfalls (coming from Rust / TS / Python)

| Lex | Meaning | Common mistake |
|---|---|---|
| `x :: Int` | type annotation | `x: Int` (Lex wants `::`) |
| `let x := e` | let binding | `let x = e` (Lex wants `:=`) |
| `-> T` | return type | `: T` (missing the arrow) |
| `[net]` before `->`'s type | effect row | putting effects after the type |

`Str + Str` concatenates. Type aliases can't be recursive — use a flat
representation + recursion in functions.

## Structured errors

`lex --output json check` emits one JSON error per line:

```json
{
  "kind": "type_error",
  "rule_tag": "EFFECT_NOT_DECLARED",
  "position": {"file": "src/handler.lex", "line": 14, "col": 22},
  "rule_explanation": "effect `fs_write` reached here is not declared.",
  "suggested_transform": { "kind": "narrow_path", "param": "path", "value": "/tmp/handler-output/" }
}
```

The `rule_tag` + `suggested_transform` are exactly what `lex repair --apply`
consumes. Exit code is `0` on success, `1` on type errors, `9` on a
`--dry-run` plan.

## Pre-"done" checklist

- [ ] Every signature declares the **narrowest** effect set (+ path/host scopes).
- [ ] Every pure fn has an `examples {}` block (or a one-line "why not").
- [ ] No `_ =>` arms outside genuine catch-alls.
- [ ] Stdlib used over roll-your-own.
- [ ] Any `lex check` failure was handled with `lex repair --apply`, not a regen.
- [ ] No SigId churn from cosmetic edits.
- [ ] `lex ci` is green.

## Remote: the lex-hub MCP tools

When the work targets a **hosted** lex-hub gateway rather than a local
store, this plugin also provides MCP tools (server `lex-hub`):

| Tool | Use |
|---|---|
| `lex_health` | Confirm the gateway is reachable. |
| `lex_check` | Type-check source for the tenant (same structured errors). |
| `lex_publish` | Publish functions into the tenant's store. |
| `lex_run` | Type-check + run a function (gateway clamps effects to pure + time/rand). |
| `lex_patch` | Apply a typed transform to a stage (the repair path). |
| `lex_stage_attestations` | Walk a stage's attestation chain. |

These need the `lex-hub-mcp` binary on `PATH` and `LEXHUB_URL` + credentials
set — see the plugin README. The same idiom rules above apply; the only
difference is the store lives behind JWT auth on a server.

## Where to read more

- `lex agent-guidelines` — the authoritative rule list.
- `docs/AGENT.md` (in lex-lang) — error envelope schema, every `rule_tag`,
  the full stdlib surface, known sharp edges.
- `README.md` — the agent-code loop overview + CLI reference table.
