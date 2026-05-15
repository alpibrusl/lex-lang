# Idiomatic Lex for AI Agents

> **Audience.** This doc is for AI agents (Claude Code, Cursor, Aider, Codex,
> Copilot, …) writing or modifying Lex code. Humans should read `README.md`
> first.
>
> **Status.** Authoritative — the rules in this file are the project
> conventions for any Lex codebase. Rule numbers are stable; new rules
> append.
>
> **Discovery.**
> ```sh
> lex agent-guidelines                # print this doc to stdout
> lex agent-guidelines > AGENTS.md    # capture into a downstream repo
> ```

The Lex toolchain encodes a lot of opinions the type checker enforces and
a few more it doesn't. This doc is the second list — the discipline that
makes Lex code idiomatic, auditable, and cheap for the next agent to
extend. Skim once, then refer back when you write.

**Semantics:**

- **MUST** — non-negotiable. Agents that violate land code that's
  type-correct but expensive to maintain.
- **SHOULD** — strong preference. Override only with a one-line comment
  explaining why.
- **AVOID** — common anti-pattern. The fix is usually trivial; just don't.

---

## 1. Effect discipline

The single highest-leverage thing you can do. Effect annotations are how
the runtime decides whether to refuse a body before any code runs. Wide
annotations are correct but worthless; the value of the effect system is
exactly the narrowness.

### 1.1 Declare the narrowest effect set (MUST)

```lex
# AVOID — shotgun annotation
fn write_log(line :: Str) -> [io, fs_read, fs_write, net] Result[Unit, Str] {
  fs.write("/var/log/app.log", line)
}

# GOOD — only what's used, with a path scope
fn write_log(line :: Str) -> [fs_write("/var/log/app.log")] Result[Unit, Str] {
  fs.write("/var/log/app.log", line)
}
```

The compiler will *accept* the shotgun form — but `lex run` requires
broader `--allow-effects` grants and reviewers can't tell what the fn
actually touches.

### 1.2 Don't broaden effects to satisfy the compiler (MUST)

When `lex check` says "effect `fs_write` not declared at line X," the
fix is **almost never** "add `fs_write` to the signature." It's "remove
the code that needed `fs_write`," or "narrow the path scope to where the
write actually happens."

If a function is supposed to be pure and the checker says otherwise,
**something is wrong with the body**, not the signature.

### 1.3 Pure functions declare no effects (MUST)

```lex
# GOOD — no [...] before the return type
fn double(n :: Int) -> Int { n * 2 }

# AVOID — adding [io] "just in case" or "because the caller is impure"
fn double(n :: Int) -> [io] Int { n * 2 }   # ← compiler will complain anyway
```

Purity is contagious in the right direction: pure fns can be called
from anywhere, including from `examples {}` blocks, `spec {}` blocks,
and inside `lex repair`'s safety-bounded environment.

### 1.4 Use per-path / per-host scopes wherever you can (SHOULD)

```lex
# GOOD — fs_read scoped to a specific directory
fn load_config(name :: Str) -> [fs_read("/etc/myapp/")] Result[Str, Str] { ... }

# GOOD — net scoped to a specific host
fn fetch_weather(zip :: Str) -> [net("api.weather.example")] Result[Str, Str] { ... }
```

The runtime enforces these scopes; per-fn `--allow-fs-read PATH` and
`--allow-net-host HOST` grants do too. Wide scopes are auditable as
"this function could touch anything"; narrow scopes are auditable as
"this function touched what it claimed."

### 1.5 Effect rows propagate through closures and HOFs (informational)

You don't need to copy effects from a closure body to the surrounding
fn signature manually — `list.map` and friends carry the closure's
effects in their inferred row, and `lex check` propagates them. If the
checker complains, the fix is to narrow the *body*, not to widen the
outer signature.

---

## 2. Authoring style

### 2.1 Every pure fn gets an `examples {}` block (SHOULD)

```lex
fn add(x :: Int, y :: Int) -> Int
  examples {
    add(0, 0)   => 0,
    add(2, 3)   => 5,
    add(-1, 1)  => 0,
  }
{
  x + y
}
```

Examples are folded into the canonical AST, so they're **part of the
SigId**. Two implementations with different example sets are different
signatures; this makes them load-bearing regression tests at no
authoring cost.

Effectful fns can't carry examples in v1 (determinism rule). See
`#369` for the design.

### 2.2 Use `spec {}` for behavioural contracts (SHOULD when behaviour matters)

```lex
spec clamp_bounds {
  forall x :: Int, lo :: Int, hi :: Int where lo <= hi:
    let r := clamp(x, lo, hi)
    (r >= lo) and (r <= hi)
}
```

Then `lex spec check clamp_bounds.spec --source clamp.lex` randomised-property checks
it; `lex spec smt` emits SMT-LIB 2 for external Z3.

### 2.3 Use `Result[T, E]` and `Option[T]` — no exceptions, no null (MUST)

```lex
# GOOD
fn parse_int(s :: Str) -> Result[Int, ParseError] { ... }

# AVOID — no exceptions in user code; the language doesn't have them
fn parse_int(s :: Str) -> Int { throw "..."}   # ← won't compile
```

Idiom: `match res { Ok(x) => ..., Err(e) => ... }` or pipe through
`result.map` / `result.and_then` / `result.map_err`.

### 2.4 Pipe over nested match (SHOULD)

```lex
# AVOID — nested matches
match parse_int(s) {
  Ok(n) => match double(n) {
    Ok(m) => Ok(m + 1),
    Err(e) => Err(e),
  },
  Err(e) => Err(e),
}

# GOOD — pipe + and_then
parse_int(s)
  |> result.and_then(double)
  |> result.map(fn (m :: Int) -> Int { m + 1 })
```

### 2.5 Exhaustive matches; no `_ =>` to silence (AVOID `_ =>` when not catch-all)

```lex
type Status = Healthy | Sick | Recovering

# AVOID — adding `_ =>` swallows a new variant added later
match s {
  Healthy => "ok",
  _       => "not ok",          # ← Recovering gets bucketed silently
}

# GOOD — listed exhaustively; the checker enforces it
match s {
  Healthy    => "ok",
  Sick       => "nope",
  Recovering => "wait",
}
```

`_ =>` is fine when you genuinely want a catch-all (e.g., the right-hand
side of a `try`); reserve it for that case.

### 2.6 Syntax pitfalls — `::`, `:=`, `->` (MUST)

Coming from Rust / TS / Python? These differ:

| Lex | Meaning | Mistake to avoid |
|---|---|---|
| `x :: Int` | type annotation on a param / binding | `x: Int` (Lex error: expected `::`) |
| `let x := e` | let binding | `let x = e` (Lex error: expected `:=`) |
| `-> T` | return type | `: T` (Lex error: missing return arrow) |

The checker error message is clear when you slip; this rule is here so
you don't have to slip in the first place.

---

## 3. Module choice — use the stdlib

Lex's stdlib is the slice the language owns. Whenever you reach for raw
bytes, a syscall, or string-bashing — check the stdlib first.

### 3.1 Concurrency: `std.conc` actors, never spawn threads (MUST)

```lex
import "std.conc" as conc

fn counter(state :: Int, msg :: Int) -> (Int, Int) {
  let next := state + msg
  (next, next)
}

fn use_counter() -> [concurrent] Int {
  let a := conc.spawn(0, counter)
  conc.ask(a, 5)            # state becomes 5, reply 5
}
```

`conc.spawn` / `conc.ask` / `conc.tell` are the actor primitives.
Synchronous mailbox — handler runs on the caller's thread under a
per-actor mutex. **Do not** try to spawn OS threads via FFI; you can't,
and you don't need to.

### 3.2 SQL: `std.sql` for SQLite and Postgres (MUST)

```lex
import "std.sql" as sql

fn pg_users(conn :: Str) -> [sql, fs_write] Result[List[{ id :: Int, name :: Str }], SqlError] {
  match sql.open(conn) {                # "postgres://...", ":memory:", or file path
    Ok(db) => sql.query(db, "SELECT id, name FROM users ORDER BY id", []),
    Err(e) => Err(e),
  }
}
```

Use parameterised queries (`sql.query(db, "... WHERE id = $1", [id])`),
never string-concat into SQL.

### 3.3 Crypto: `std.crypto`, never roll your own (MUST)

`std.crypto` ships SHA-256/512, BLAKE2b, MD5, HMAC, AES-GCM,
ChaCha20-Poly1305, PBKDF2, HKDF, Argon2id, base64, base64url, hex, and
constant-time `eq`. CSPRNG is `crypto.random(n)` under `[random]`.

Hand-rolling MACs, AEADs, or "XOR with a key" is wrong every time.

### 3.4 Strings, regex, parsing — stdlib first (SHOULD)

- `std.regex` over hand-rolled scanners
- `std.str.split` / `.replace` / `.trim` over manual loops
- `std.toml` / `std.yaml` / `std.csv` for config + tabular data
- `std.json` (via `lex-types::builtins`) for JSON

### 3.5 Orchestration: `std.flow` combinators (SHOULD)

`flow.sequential` / `flow.branch` / `flow.parallel_list` over ad-hoc
control flow. They compose cleanly and the closures carry their effects
through the row inference.

### 3.6 HTTP: `std.http` with per-host scopes (MUST for net)

```lex
import "std.http" as http

fn fetch_json(url :: Str) -> [net] Result[HttpResponse, HttpError] {
  http.get(url)
}
```

Always pair with `--allow-net-host` at the policy gate. Never construct
raw sockets.

---

## 4. The repair-not-regenerate rule

When `lex check` rejects your code, you have two paths:

1. Throw away the body and regenerate.
2. Apply `lex repair --apply` with the structured fix the checker
   suggested.

**Always try path 2 first.** Path 1 burns budget and produces churn
that's hostile to merge / blame / spec.

### 4.1 On type-check failure, run `lex check --output json` (MUST)

```json
{
  "kind": "type_error",
  "rule_tag": "EFFECT_NOT_DECLARED",
  "position": {"file": "src/handler.lex", "line": 14, "col": 22},
  "rule_explanation": "effect `fs_write` reached at this position is not declared in the enclosing fn signature.",
  "suggested_transform": { "kind": "narrow_path", "param": "path", "value": "/tmp/handler-output/" }
}
```

The `rule_tag` and `suggested_transform` are what `lex repair --apply`
consumes.

### 4.2 If the error has a `suggested_transform`, apply it verbatim (SHOULD)

```bash
lex --output json repair <failed_op_id> \
  --apply --transform '<paste the suggested_transform here>' \
  --store .lex-store
# → {"outcome":"passed","applied_op_id":"op_..."}
```

The output lands as a `RepairAttempt` attestation linked to the
originating hint. Future agents (and `lex blame --with-evidence`) can
follow the chain.

### 4.3 Only regenerate the whole body after 2 failed repairs (SHOULD)

If two `lex repair --apply` attempts fail, the structural fix doesn't
exist — the body's design is wrong, not its syntax. Regenerate the body
*intentionally* (not as a reflex), keeping the signature stable so the
SigId — and the existing attestations — survive.

---

## 5. Multi-agent coordination

Many agents may edit the same code in parallel. Lex provides primitives
for this; use them.

### 5.1 Use `Candidate` / `Promote` for parallel emit (SHOULD)

Instead of racing to overwrite a branch head, propose `Candidate` ops
that don't advance the head; a downstream policy `Promote`s the winner.

```bash
# Each proposer pushes a candidate; head is unchanged.
lex publish --candidate --branch main src/handler.lex

# Selector picks one (manually or via `lex producer-trust recompute`).
lex stage promote-candidate <candidate_op_id> --branch main
```

CAS contention drops to zero. Each candidate is attested
independently; the loser candidates remain in the log for replay.

### 5.2 Respect the session budget gate (MUST)

```http
HTTP/1.1 503 Service Unavailable
Retry-After: 0
{"error":"session 'sid_xyz' budget exceeded (spent_after=450, cap=400)", ...}
```

`Retry-After: 0` is **not** "retry in 0 seconds." It's "don't retry
as-is; either raise the cap, refactor to spend less, or stop." Auto-retry
on this response makes the cap meaningless.

### 5.3 Tool registry manifests match real effects (MUST)

When you register a tool via `POST /tools`:

```json
{
  "name": "weather-fetcher",
  "effects": ["net('api.weather.example')"],
  "source": "fn fetch(zip :: Str) -> ..."
}
```

The declared `effects` field MUST match what the source's type-checked
signature claims. Overdeclaring is bad; underdeclaring means the tool
will fail at runtime in places callers didn't expect.

---

## 6. Hash and store hygiene

The store keys on the canonical AST hash (SigId). Cosmetic changes that
shift hashes are expensive — they invalidate attestations and force
re-attest cycles.

### 6.1 Run `lex fmt` before publish (MUST)

```bash
lex fmt src/        # auto-format
lex fmt --check src/  # exit 1 if not formatted
```

`lex fmt` is canonical; two semantically-identical files normalise to
byte-identical output.

### 6.2 Don't reformat for "readability" (AVOID)

If `lex fmt` produces output you don't like, file an issue; don't
hand-rewrite into a non-canonical form. Hand-formatting that gets
through `lex fmt --check` is a no-op anyway.

### 6.3 Don't rename params/locals cosmetically (AVOID)

Parameter and local names are part of the canonical AST.
`fn add(x :: Int, y :: Int)` and `fn add(a :: Int, b :: Int)` have
**different SigIds**. Rename only when the name was actually wrong.

### 6.4 Don't reorder top-level fns cosmetically (AVOID)

Top-level fn order is preserved by the canonicalizer. Reordering shifts
the canonical AST; downstream tooling that keyed on SigIds may break.

See `docs/design/canonicalization.md` for the full list of edits that
preserve vs change a SigId.

---

## 7. Attestation hygiene

Attestations are signed evidence that a gate (typecheck, spec, sandbox,
examples, repair) covered a stage. The substrate emits them; you query
them.

### 7.1 Don't fabricate attestations (MUST)

Attestations come from the gates (`lex check` → `TypeCheck`,
`lex spec check` → `Spec`, `lex agent-tool` → `SandboxRun`, …). You
cannot — and must not try to — write them directly. The signing
boundary is what makes them load-bearing.

### 7.2 Query attestations via `lex blame --with-evidence` (SHOULD)

Before modifying a fn, check its evidence trail:

```bash
lex blame route --with-evidence --store .lex-store
```

Surfaces every TypeCheck / Spec / Examples / DiffBody / SandboxRun /
RepairAttempt that touched the fn's stages. If you're about to redo
work the substrate already attested, stop.

### 7.3 `lex stage <id> --attestations` for a single stage (informational)

Same query, scoped to one stage. Use when reviewing a `Candidate` you're
considering promoting.

---

## 8. What NOT to do

A short blocklist. Every one of these is a real anti-pattern AI agents
emit. Avoid.

| Pattern | Why it's wrong | Do instead |
|---|---|---|
| `_ => "ok"` outside a try/catch-all | Swallows new variants silently | List exhaustively |
| Hand-rolled crypto | One bug = full compromise | `std.crypto` |
| String-concat SQL | Injection | `sql.query(db, q, [args])` |
| Wide `[net, io, fs_read, fs_write]` | Sandbox is meaningless | Narrow to what's used + path scopes |
| `[io]` "just in case" | Pure fn won't typecheck anyway | Remove it |
| Renaming params after publish | Different SigId; loses attestations | Don't |
| Auto-retry on HTTP 503 from `lex serve` | Budget cap is intentional | Raise cap or refactor |
| Mutation via FFI | No FFI; no mutation in user code | Build new values |
| Spawning OS threads | Not supported; not needed | `std.conc` actors |
| Bypassing the gate via `proc.exec` | Defeats the sandbox model | Don't; if you need OS access, declare `[proc]` explicitly |

---

## 9. Pre-"done" checklist

Before claiming a task is complete, run all of:

```bash
lex check --strict src/        # type-check with extra lints
lex fmt --check src/ tests/    # formatting
lex test                        # all tests/test_*.lex
lex ci                          # umbrella — same as above
```

Then verify by inspection:

- [ ] Every fn signature declares the **narrowest** effect set.
- [ ] Every pure fn has an `examples {}` block (or a one-line comment
      explaining why not — usually "trivial accessor").
- [ ] No `_ =>` arms outside catch-all error paths.
- [ ] Stdlib used in preference to roll-your-own.
- [ ] If you saw a `lex check` error during the task, you ran
      `lex repair --apply` rather than regenerating the body.
- [ ] No SigId churn from cosmetic edits (param renames, fn reordering).
- [ ] Tool-registry manifests, if any, match the actual effect rows.

---

## 10. Where to read more

- **`README.md`** — high-level pitch + agent-code loop overview.
- **`docs/AGENT.md`** — reference: error envelope schema, every
  `rule_tag`, stdlib module summary, sharp edges.
- **`docs/design/canonicalization.md`** — which edits preserve a SigId
  and which break it.
- **`bench/REPORT.md`** — adversarial sandbox bench + infrastructure
  comparison.
- **`crates/lex-types/src/builtins.rs`** — every stdlib signature, the
  source of truth.

For an issue you can't resolve: file at
<https://github.com/alpibrusl/lex-lang/issues>, include the
`lex check --output json` envelope verbatim.
