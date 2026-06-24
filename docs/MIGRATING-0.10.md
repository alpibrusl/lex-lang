# Migrating to Lex 0.10.0

0.10.0 is a stdlib-audit release. It carries **three breaking changes**.
The good news: **all three are caught at `lex check` time** (an unresolved
import, an effect-row mismatch, or a type mismatch) — nothing breaks
silently at runtime. Migration is mechanical, and `lex check` is your
checklist: fix what it flags and you're done.

Downstream repos pin a fixed `lex` version, so nothing upgrades until you
bump the pin deliberately. When you do, work through the three items below.

---

## 1. `std.proc` removed — use `std.process` (#678)

`std.proc` exposed a single op that was byte-for-byte equivalent to
`process.run`. It's gone; `std.process` is a strict superset.

**Symptom:** `import "std.proc"` no longer resolves (`lex check` error).

**Fix — drop-in rename:**

```diff
- import "std.proc" as proc
+ import "std.process" as process

- proc.spawn(cmd, args)
+ process.run(cmd, args)
```

Same `[proc]` effect, same `{ stdout, stderr, exit_code }` result, same
`--allow-proc` binary allow-list. The `[proc]` effect itself is unchanged.
(For streaming, `std.process` also offers `spawn` / `read_*_line` / `wait`
/ `kill`.)

## 2. `rand.int_in` — honest randomness under `[random]` (#677)

It previously declared `[rand]` but returned the deterministic midpoint
`(lo + hi) / 2` — a constant. It now draws a real uniform integer in
`[lo, hi]` from the OS RNG, gated by the same `[random]` effect as
`crypto.random`. **There are two changes to make:**

**a) Effect rename** (`lex check` flags the mismatch):

```diff
- fn pick(...) -> [rand] Int { rand.int_in(0, n) }
+ fn pick(...) -> [random] Int { rand.int_in(0, n) }
```

and at the policy gate: `--allow-effects rand` → `--allow-effects random`.
The standalone `rand` effect no longer exists.

**b) Behaviour** — it's now actually random. If you depended on the old
deterministic midpoint (e.g. to get stable test output), switch to:

- **`std.random`** — seeded, pure, replayable (thread an `Rng` value); the
  idiomatic choice for deterministic randomness, or
- **`crypto.random`** — cryptographically secure, also `[random]`.

## 3. `http.stream_lines` returns `Stream[Str]`, not `Iter[Str]` (#683)

The old `Iter` was eager: it buffered the whole response before returning,
so an SSE endpoint that holds the connection open hung. It now returns a
lazy `Stream[Str]` read line-by-line off the socket.

**Symptom:** a type error where the result was consumed with `iter.*`.

**Fix — consume with `std.stream`:**

```diff
+ import "std.stream" as stream

  match http.stream_lines(url, headers, body) {
-   Ok(it) => iter.to_list(it),
+   Ok(s)  => stream.collect(s),     # or pull lazily with stream.next(s)
    Err(e) => ...,
  }
```

Add the `[stream]` effect to the consuming fn (and `--allow-effects
stream`). The new behaviour is strictly better — lines arrive
incrementally as the server sends them, with no buffering and no hang on
open-ended streams.

---

## Worth reviewing (not a hard break)

- **`http.json_body[T]` now validates (#684).** When `T` is a record, a
  missing or wrong-typed field now returns `Err(DecodeError)` instead of an
  `Ok` with a silently-incomplete record (which used to panic at the later
  field access). If you handle the `Result` properly this is purely an
  improvement; just make sure your `Err` arm is meaningful.
- **`NET_SERVE_NAMED` lint (#680).** `lex check --strict` now warns on the
  name-based `net.serve` / `serve_tls` / `serve_ws` / `serve_with` /
  `serve_quic` forms (handler passed by name, effect row unchecked) and
  points you at the closure variants (`serve_fn` / `serve_routed` /
  `serve_ws_fn`). It's a `--strict` **warning**, not an error — a plain
  `lex check` is unaffected.

## New, no action needed

`result.unwrap_or` / `unwrap_or_else` / `is_ok` / `is_err`, `option.is_some`
/ `is_none` / `ok_or` (#679); `int.min` / `max` / `abs`,
`duration.millis` / `minutes` / `hours` / `days` (#681); and the
`std.yaml` `Option[T]` decoding fix (#682). See `CHANGELOG.md` for the full
list.

- **Effect-row polymorphism (`[base | E]`).** Functions can now carry an open
  effect-row tail naming a type parameter, so a generic helper or server
  forwards a callback/handler's extra effects instead of fixing a concrete
  row. Purely additive: closed rows are unchanged (still equality), and
  existing content hashes (`lex hash`) are byte-identical. Adoption guide with
  before/after in
  [`docs/effect-row-polymorphism.md`](effect-row-polymorphism.md).
