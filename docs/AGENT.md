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
`ends_with`, `replace`, `concat`, `slice`, `index_of`

String comparison operators (`<`, `<=`, `>`, `>=`, `==`, `!=`) work on `Str`
via lexicographic order.

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

### `std.http`
`get`, `post`, `put`, `delete`, `patch` — all require `[http.*]` effect.

### `std.net`
`net.serve(port, handler_name)` — HTTP server, handler looked up by name string.
`net.serve_fn(port, handler)` — HTTP server with a first-class closure handler:
```lex
fn my_handler(req :: Request) -> [io] Response {
  { status: 200, body: "ok", headers: map.new() }
}
fn main() -> [net, io] Nil { net.serve_fn(8080, my_handler) }
```
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
