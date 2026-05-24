# Lex — Agent Cold-Start Guide

This document is written for AI agents that are building or maintaining Lex programs.
It covers the error envelope format, the iteration loop, effect annotations, stdlib surface,
and known sharp edges — everything you need to be productive from a cold start.

---

## Project layout

```
crates/
  lex-syntax/      parser
  lex-ast/         canonical AST + canonicalizer
  lex-types/       type checker + builtin signatures
  lex-bytecode/    compiler + stack-machine VM
  lex-runtime/     effect handler, DefaultHandler, Policy
  lex-stdlib/      (reserved; builtins are currently inline in lex-types/lex-runtime)
  lex-cli/         `lex` binary — all subcommands
docs/              this file + design docs
```

Source files use the `.lex` extension. Entry points are named top-level functions.

---

## Iteration loop

```
write program.lex
  → lex check program.lex --output json    # type-check; get structured errors
  → lex run program.lex my_fn '"arg"'      # run one function
  → lex test                               # run all tests/test_*.lex files
```

Always run `lex check` before `lex run`. The type checker catches most mistakes;
the runtime only sees what the compiler emits.

---

## `lex check` error envelope

Every error is a JSON object on stdout (one per line with `--output json`):

```json
{
  "kind":             "type_error",
  "expected":         "Int",
  "got":              "Str",
  "at_node":          "fn_body/call/arg[0]",
  "position":         { "line": 12, "col": 5 },
  "rule_tag":         "PARAM_TYPE_MISMATCH",
  "rule_explanation": "argument 0 of `validate` expects Int, got Str"
}
```

Fields:

| field | meaning |
|---|---|
| `kind` | always `"type_error"` |
| `expected` | the type the checker required |
| `got` | the type it found |
| `at_node` | AST path to the failing node |
| `position` | source location |
| `rule_tag` | machine-readable tag for filtering |
| `rule_explanation` | human-readable sentence |

Exit code is 0 on success, 1 on type errors.

---

## Effect system quick reference

Functions declare effects in their signature:

```lex
fn fetch_data(url :: Str) -> Str [http.get] { ... }
```

The effect kind (`http`) and operation (`get`) must both be permitted by the
active `Policy`. Effect kinds that exist:

| kind | operations | notes |
|---|---|---|
| `http` | `get`, `post`, `put`, `delete`, `patch` | outbound HTTP |
| `fs` | `read`, `write` | filesystem access |
| `time` | `now` | `datetime.now` — non-deterministic |
| `random` | `random` | `crypto.random` |
| `llm` | `complete` | LLM inference |
| `budget` | (N) | annotated cost; checked against `--budget` |

Pure functions (no effect annotations) can run under `Policy::pure()`.

---

## Stdlib module summary

Import modules with `import "std.X" as X`:

### `std.str`
`length`, `to_upper`, `to_lower`, `trim`, `split`, `contains`, `starts_with`,
`ends_with`, `replace`, `concat`, `slice`, `index_of`, `cmp`

String comparison operators (`<`, `<=`, `>`, `>=`, `==`, `!=`) work on `Str`
via lexicographic order. `str.cmp(a, b)` returns `-1` / `0` / `1` in the
same order — use it when you need a three-way function value (e.g. as a
sort-by closure); use the operators for boolean comparisons.

`Str + Str` concatenates strings.

### `std.int`
`to_str`, `parse`, `abs`, `min`, `max`, `clamp`

### `std.list`
`map`, `filter`, `fold`, `length`, `head`, `tail`, `reverse`, `append`,
`zip`, `flatten`, `any`, `all`, `find`, `cons`, `par_map`

`list.cons(x, xs)` prepends `x` to `xs` — idiomatic O(n) builder with
`list.cons` + `list.reverse`.

### `std.json`
`stringify`, `parse` — round-trips any Lex value through JSON.

### `std.datetime`
`now` [time], `parse_iso`, `format_iso`, `diff`, `before`, `after`, `compare`

`datetime.now` returns nanoseconds since epoch as `Int`.
`datetime.parse_iso` returns `Result[Instant, Str]`.
`datetime.diff(later, earlier)` returns a `Duration`.
`datetime.compare(a, b)` returns `-1`, `0`, or `1`.

Set `LEX_TEST_NOW=<unix_seconds>` to pin the clock in tests (#350).

### `std.duration`
`seconds`, `minutes`, `hours`, `days`

`duration.seconds(d)` extracts total whole seconds from a `Duration`.

### `std.time`
`now` [time], `now_ms` [time], `now_str` [time], `mono_ns` [time],
`sleep_ms` [time], `sleep` [time]

`time.sleep(d :: Duration)` blocks the calling thread for `d`. Pair with
`datetime.duration_seconds` / `duration_minutes` / `duration_days` to
express the period in units. Inside a `net.serve` worker the sleep
stalls the worker — same caveat as `LEX_NET_INLINE_VM=1`. Capped at 60s
to bound runaway loops; use process-level scheduling for longer waits.

### `std.conc`
`spawn` [concurrent], `ask` [concurrent], `tell` [concurrent],
`register` [concurrent], `lookup` [concurrent], `unregister` [concurrent],
`registered` [concurrent]

Actors hold per-process state across messages. `spawn(init, handler)`
returns `Actor[S]`; `ask` / `tell` run `handler(state, msg)` on the
caller's VM thread, serialised by the actor's internal mutex.

`register(actor, name)` makes the actor reachable by name from anywhere
in the process — for routing a request handler to the actor that owns
the relevant agent state (vehicle / depot / etc.). Returns `Err(AlreadyRegistered(name))`
if the name is taken. `lookup(name) :: Option[Actor[S]]` retrieves it;
the `[S]` is parametrised at the call site and trusted to match the
registration site (no runtime type check in v1). `unregister(name)`
drops the name binding but existing `Actor[S]` handles held by callers
keep working. `registered()` returns a sorted snapshot of names.

### `std.http`
`get`, `post`, `put`, `delete`, `patch` — all require `[net]` effect.

`stream_lines(url :: Str, headers :: Map[Str, Str], body :: Str) -> [net] Result[Iter[Str], Str]`
— HTTP POST that reads the full response body and yields it split into lines as `Iter[Str]`.
Designed for LLM provider APIs (OpenAI, Anthropic, Google) that use SSE or NDJSON and
**close the connection** after sending all events. Requires `[net]` effect.

**WARNING — eager buffer, not true streaming.** The current implementation (ureq 3.3) reads
the entire response body into memory before splitting into lines. This means:
- It blocks until the server closes the connection. Endpoints that hold connections open
  indefinitely (traditional push-SSE feeds, infinite event streams) will hang forever.
- The full response must fit in memory.

Use only with endpoints that terminate the connection after sending all data.

### `std.net`
`net.serve(port, handler_name)` — HTTP server, handler looked up by name string.
`net.serve_fn(port, handler)` — HTTP server with a first-class closure handler:
```lex
fn my_handler(req :: Request) -> [io] Response {
  { status: 200, body: "ok", headers: map.new() }
}
fn main() -> [net, io] Nil { net.serve_fn(8080, my_handler) }
```

`net.serve_with(port, handler_name, opts)` / `net.serve_fn_with(port, handler, opts)` / `net.serve_routed_with(port, routes, fallback, opts)` — same as the unsuffixed variants but accept a `ServeOpts` record literal (`{ http2: Bool, inline_vm: Bool, host: Str }`) instead of relying on `LEX_NET_HTTP2` / `LEX_NET_INLINE_VM` env vars. `net.default_opts()` returns the defaults (`http2: false, inline_vm: false, host: "0.0.0.0"`); construct your own literal to enable HTTP/2 or bind to a specific host. The legacy `serve` / `serve_fn` / `serve_routed` paths keep honouring the env vars for backwards compatibility — new code should prefer the `*_with` variants (#497).

`net.serve_quic(port, tls, handler_name)` / `net.serve_quic_fn(port, tls, handler)` / `net.serve_quic_routed(port, tls, routes, fallback)` — HTTP/3 over QUIC (#496). TLS is mandatory; `tls` is a `TlsConfig` opaque value built via `std.tls` (see below). Requires the `quic` feature on lex-runtime: `cargo build --features quic` (off by default to keep the dep graph slim). HTTP/3 negotiates via the `h3` ALPN over UDP; existing HTTP/1.1+2 (TCP) listeners aren't affected. Effect row stays `[net]` — same gate as `serve` / `serve_fn`. 0-RTT is disabled by default (replay-attack risk on non-idempotent handlers); cert rotation requires a restart in v1.

```lex
import "std.net" as net
import "std.tls" as tls

fn handle(req :: Request) -> Response { ... }

fn main() -> [net] Nil {
  match tls.self_signed("localhost") {
    Ok(t) => net.serve_quic(4433, t, "handle"),
    Err(_) => (),
  }
}
```

### `std.tls`
`tls.from_pem_files(cert_path, key_path) -> [fs_read] Result[TlsConfig, Str]` — load a PEM-encoded certificate chain + private key from disk. Both paths must be under `--allow-fs-read`.
`tls.self_signed(hostname) -> Result[TlsConfig, Str]` — generate a self-signed certificate for the given hostname. Pure (no effects). Intended for local development and tests; production should use a CA-signed cert via `from_pem_files`.

`TlsConfig` is opaque — the only ways to obtain one are these two constructors. Pass it to `net.serve_quic*` to set up an HTTP/3 listener.

`net.serve_ws_fn(port, subprotocol, handler)` — WebSocket server:
```lex
fn on_msg(conn :: WsConn, msg :: WsMessage) -> WsAction {
  match msg {
    WsText(s) => WsSend("echo: " + s),
    _         => WsNoOp,
  }
}
fn main() -> [net] Nil { net.serve_ws_fn(9000, "", on_msg) }
```
Types: `Request`, `Response`, `WsConn`, `WsMessage`, `WsAction` are global (no import needed).

`net.dial_ws(url, subprotocol, on_open, on_message)` — reactive WebSocket client. `on_open` is called once after the handshake; `on_message` is called for each inbound frame. Both return a `WsAction` (`ws.send(frame)` / `ws.noop()`). Blocks for the lifetime of the connection; call from `conc.spawn`.

`net.dial_ws_actor(url, subprotocol, name, on_open, on_message)` — like `dial_ws` but also registers the live connection in the `conc` registry under `name`. Any actor can push frames proactively via `conc.tell(conc.lookup(name), frame_str)` — the runtime forwards them to the socket on the next read-timeout tick (50 ms). Use for CP simulators, heartbeat loops, and any client that needs to send unsolicited frames without changing the `on_message` signature. Unregisters automatically on disconnect.
```lex
import "std.conc" as conc
import "lex-web/ws" as ws

fn run(url :: Str) -> [net, time, concurrent] Result[Unit, Str] {
  net.dial_ws_actor(url, "ocpp1.6", "cp:001",
    fn () -> [time, concurrent] WsAction { ws.send("[2,\"1\",\"BootNotification\",{}]") },
    fn (m :: WsMessage) -> [time, concurrent] WsAction {
      match m { WsText(_) => ws.noop(), _ => ws.noop() }
    })
}
```

### `std.crypto`
`hash`, `random` — `random` requires `[random]` effect.

### `std.arrow`
Apache Arrow `RecordBatch` as a first-class `Value::ArrowTable`. Column
reductions and slicing run as one Rust call over the flat buffer, bypassing
the bytecode VM for the inner loop (orders-of-magnitude faster than a
`List[Value]` walk).

Constructors (all return `Result[Table, Str]`; length-mismatch is `Err`, not panic):
- `from_int_columns   :: List[(Str, List[Int])]   -> Result[Table, Str]`
- `from_float_columns :: List[(Str, List[Float])] -> Result[Table, Str]`
- `from_str_columns   :: List[(Str, List[Str])]   -> Result[Table, Str]`

Introspection:
- `nrows`, `ncols :: Table -> Int`
- `col_names :: Table -> List[Str]`
- `col_type  :: (Table, Str) -> Option[Str]` (`"int64" | "float64" | "utf8"`)

Reductions:
- `col_sum_int   :: (Table, Str) -> Result[Int, Str]`
- `col_sum_float :: (Table, Str) -> Result[Float, Str]`
- `col_mean      :: (Table, Str) -> Result[Option[Float], Str]` (`None` on empty)
- `col_min_int`, `col_max_int :: (Table, Str) -> Result[Option[Int], Str]`
- `col_count    :: (Table, Str) -> Result[Int, Str]` (non-null count)

Slicing (all zero-copy via `RecordBatch::slice` / `project`):
- `head`, `tail :: (Table, Int) -> Table`
- `slice :: (Table, Int, Int) -> Table`
- `select_cols :: (Table, List[Str]) -> Result[Table, Str]`
- `drop_col   :: (Table, Str) -> Result[Table, Str]`

I/O (effect-gated):
- `read_csv :: Str -> [fs_read] Result[Table, Str]` — header row required;
  schema inferred from the first 100 rows.

### `std.df`
Polars-backed query ops over `arrow.Table` (#427). All pure — the Polars
`DataFrame` is internal plumbing, never escapes the kernel; results are
returned as `Value::ArrowTable`.

Filters:
- `filter_eq_int`, `filter_gt_int`, `filter_lt_int :: (Table, Str, Int) -> Result[Table, Str]`

Sort:
- `sort_by :: (Table, Str, Bool) -> Result[Table, Str]` (`asc = true|false`)

Group + aggregate (one call):
- `group_by_agg :: (Table, List[Str], List[(Str, Str, Str)]) -> Result[Table, Str]`

  Spec tuple is `(out_col, in_col, op)`; `op ∈ "sum"|"mean"|"min"|"max"|"count"|"n_distinct"`.

Joins:
- `inner_join`, `left_join :: (Table, Table, Str) -> Result[Table, Str]`

---

## `lex.toml` — package dependencies

Projects with dependencies declare them in a `lex.toml` at the project root:

```toml
[package]
name = "my-app"
version = "0.1.0"

[dependencies]
lex-schema = { path = "../lex-schema" }
# or:
lex-schema = { git = "https://github.com/alpibrusl/lex-schema" }
```

Then import with the package name instead of a relative path:

```lex
import "lex-schema/validate" as v
import "lex-schema/schema"   as s
```

Module resolution: `{pkg_root}/src/{module}.lex`, then `{pkg_root}/{module}.lex`.

Git dependencies are cloned to `~/.lex/packages/` on first use (override with `$LEX_PACKAGES_DIR`).

Manage with `lex pkg init / add / list`.

---

## `lex test` — test runner

Place test files in `tests/` named `test_*.lex`. Each file must export:

```lex
fn run_all() -> () { ... }
```

Use `assert` (or pattern-match + panic) inside `run_all`. The runner calls
`run_all` with a permissive policy and reports pass/fail per file.

```
lex test            # runs tests/test_*.lex
lex test my/dir     # runs my/dir/test_*.lex
```

Pin time-dependent tests with `LEX_TEST_NOW`:

```bash
LEX_TEST_NOW=1700000000 lex test
```

---

## `lex repl` — interactive evaluator

```
lex repl                     # blank session
lex repl --load src/rules.lex  # pre-load a file (repeatable)
```

Meta commands inside the REPL: `.help`, `.quit`, `.reset`, `.list`.

Top-level inputs (`fn`, `type`, `import`) extend the session; anything else
is evaluated as an expression and printed via `json.stringify`.

---

## Known sharp edges

### Type-checker accepts, runtime rejects
The type checker and the runtime builtin dispatch can drift. If a function
signature is registered in `lex-types/src/builtins.rs` but its dispatch arm
is missing in `lex-runtime/src/builtins.rs`, the type checker will accept the
call but the runtime will error. When you see an unexpected runtime error on a
stdlib call, check both files.

### `datetime.now` is non-deterministic
`datetime.now` has effect kind `time`. It returns nanoseconds since epoch.
Pin it with `LEX_TEST_NOW=<unix_seconds>` for reproducible tests.

### REPL policy is permissive
The REPL runs under `Policy::permissive()` — all effects are allowed. Use
`lex run --allow-effects ...` to test under a specific policy.

### Recursive types not supported
Type aliases cannot be recursive. Use a flat representation and recursion in
functions instead.

### Pattern match must be exhaustive
The type checker requires exhaustive match arms for variants. Add a wildcard
arm (`_ => ...`) if you don't handle every case.

---

## Useful commands

```bash
lex check --output json program.lex        # structured errors
lex check --strict program.lex             # + STR_CMP / SHADOW_FN lint
lex run program.lex fn_name '"arg"' 42     # args are JSON
lex test                                   # run tests/test_*.lex
lex repl --load src/rules.lex              # interactive with project loaded
lex audit program.lex --effect http        # find all http effect calls
lex hash program.lex                       # canonical content hashes
lex publish --activate program.lex         # publish to local store
LEX_TEST_NOW=1700000000 lex test           # deterministic time in tests
lex pkg init                               # create lex.toml
lex pkg add lex-schema --path ../lex-schema  # add local dep
lex pkg add lex-schema --git https://github.com/alpibrusl/lex-schema
lex pkg list                               # show deps
```

---

## Filing issues

Repository: https://github.com/alpibrusl/lex-lang

Include:
- The `.lex` source (or a minimal reproducer)
- The `lex check --output json` output
- The `lex run` error if it's a runtime issue
- The version: `lex version`
