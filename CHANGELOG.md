# Changelog

All notable changes to lex-lang. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
versioning follows [SemVer](https://semver.org/) (pre-1.0; minor
bumps may carry breaking changes when justified).

## [Unreleased]

### Added

- **`std.sql` — embedded SQL (SQLite).** Second of the OSS-Auditor
  stdlib follow-ups. Wraps `rusqlite` with the bundled SQLite
  feature so no system lib is required. Surface:
  - `sql.open(path) -> [sql, fs_write] Result[Db, Str]` — `Db`
    is an opaque Int handle into a process-wide LRU-bounded
    registry (256-handle cap with FIFO eviction, same shape as
    `Kv`). `":memory:"` is exempt from the `--allow-fs-write`
    scope; on-disk paths must fall under it.
  - `sql.close(db) -> [sql] Nil` — explicit cleanup; the LRU cap
    bounds leaks for code that forgets.
  - `sql.exec(db, sql, params: List[Str]) -> [sql] Result[Int, Str]`
    — INSERT / UPDATE / DELETE / DDL. Returns the affected row
    count (`rusqlite::execute`).
  - `sql.query[T](db, sql, params: List[Str]) -> [sql] Result[List[T], Str]`
    — polymorphic on the row record shape, decoded column-by-
    column into a record keyed by column name. SQLite types map
    one-for-one to Lex `Value` variants: `Null → Unit`,
    `Integer → Int`, `Real → Float`, `Text → Str`, `Blob →
    Bytes`. Same shape as `json.parse` / `toml.parse`.
  - Per-handle `Arc<Mutex<…>>` lock pattern from the v1.5
    process-registry refactor — global lookup mutex held only
    during dispatch; ops on different connections don't
    serialize.
  - v1 caveats deferred to v1.5: SQL transactions (HOF), typed
    heterogeneous parameter binding (`SqlValue` variant),
    named parameters. Today's `List[Str]` surface relies on
    SQLite's column-type-affinity coercion; users stringify
    Int / Float values before binding.
- **`std.toml` — TOML config parser.** First slice of the
  `std.config` umbrella requested by the OSS Auditor team
  (priority: TOML > YAML > dotenv > CSV; TOML alone clears 80%
  of the use case). Adds:
  - `toml.parse[T](s :: Str) -> Result[T, Str]` — polymorphic on
    the parsed shape, mirroring `json.parse`. Routes through
    `serde_json::Value` so the parsed result composes with the
    existing JSON tooling and decodes into the same `Value`
    shape (Str / Int / Float / Bool / List / Record).
  - `toml.stringify[T](v :: T) -> Result[Str, Str]` — Result
    rather than Str because TOML's grammar is stricter than
    JSON's (top level must be a table; no nulls; no mixed-type
    arrays), so unrepresentable values surface as `Err` rather
    than panic.
  - TOML datetimes deserialize to RFC 3339 strings (the only
    info-losing step); callers who want an `Instant` pipe the
    string through `datetime.parse_iso`.
- **`std.http` — rich HTTP client.** Adds:
  - Wire ops: `http.send(req)`, `http.get(url)`, `http.post(url,
    body, content_type)` — all gated on `[net]` and respecting
    `--allow-net-host`.
  - Builders (pure record transforms): `http.with_header`,
    `http.with_auth(req, scheme, token)` (renders
    `Authorization: <scheme> <token>`), `http.with_query` (URL-
    encodes the params and appends `?k=v&...`), and
    `http.with_timeout_ms`.
  - Decoders: `http.text_body(resp) -> Result[Str, HttpError]` and
    polymorphic `http.json_body(resp) -> Result[T, HttpError]`
    (mirrors `json.parse`).
  - `HttpRequest` / `HttpResponse` registered as built-in record
    aliases, `HttpError = NetworkError(Str) | TimeoutError |
    TlsError(Str) | DecodeError(Str)` as a built-in variant.
    Anonymous record literals coerce to `HttpRequest` so users
    write `{ method: ..., url: ..., headers: map.new(), body:
    None, timeout_ms: None }` without a dedicated constructor.
  - Multipart upload + streaming response bodies are deferred to
    v1.5; the v1 surface covers the common cases (auth, headers,
    query, timeouts, JSON / text decoding). Closes #98.
- **`std.flow.parallel_list[T](actions: List[() -> T]) -> List[T]`** —
  variadic counterpart to `flow.parallel`. Runs each 0-arg closure
  in input order and returns the results as a list. Sequential under
  the hood (same caveat as `flow.parallel`); spec §11.2 reserves
  true threading for a future scheduler. Compiled inline (mirroring
  `list.map`) so closure args flow through `CallClosure` rather than
  a heap-allocated trampoline. Unlike `parallel`, the result is the
  list itself rather than a closure, since input arity is dynamic.
  (#116, refs #105)
- **`std.map.fold(m, init, fn (acc, k, v) -> acc')`** — three-arg
  left fold over `Map[K, V]` entries. Iteration order matches
  `map.entries` (BTreeMap-sorted by key). Effect-polymorphic on the
  combiner like `list.fold`. Compiled inline; materializes the entry
  list once via the existing `("map", "entries")` runtime op, then
  runs the same loop as `list.fold`. (#118, closes item 1 of #115)
- **Local file imports** between `.lex` modules
  (`import "./helpers" as h`, `../`, `/abs/`). Imported files'
  top-level fns and types are mangled with a stable per-file prefix
  so they don't collide with the importer's names; references —
  including `m.foo(...)` calls and `m.Type` annotations — get
  rewritten in place. Cycle detection reports the full path chain.
  Stdlib imports unchanged. Multi-file programs no longer collapse
  into a single file. (#83, closes #78)
- **`lex check` surfaces required `--allow-effects` grants** in
  both text and JSON output. The text mode adds a one-line summary
  plus a ready-to-run `lex run --allow-effects ...` suggestion;
  the JSON adds `required_effects`, `required_fs_read`,
  `required_fs_write`, and `required_net_host`. Pure programs
  stay silent on effects to keep the existing single-line `ok`
  clean. (#85, closes #81)
- `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, GitHub issue / PR
  templates, Dependabot config — open-source housekeeping.

### Changed

- **`std.datetime.to_components` now takes a typed `Tz` variant**
  (breaking) — `Utc | Local | Offset(Int) | Iana(Str)` instead of
  the prior stringly form (`"UTC"` / `"Local"` / IANA name /
  `"+05:30"`). `Tz` is registered as a built-in nominal type so
  users mention `Utc` / `Iana("America/New_York")` without an
  import; a typo in `Utc` / `Local` is now caught at type-check
  time rather than producing a runtime "unknown timezone" error.
  Migration: `to_components(t, "UTC")` → `to_components(t, Utc)`;
  `"+05:30"` → `Offset(330)` (minutes east of UTC). (#122, closes
  item 6 of #115)
- **`Value::Bytes` now round-trips through JSON via the `$bytes`
  marker** (breaking on the wire). `to_json` emits
  `{"$bytes": "deadbeef"}` instead of a bare lowercase-hex string,
  mirroring the existing `$variant` / `$f64_array` shapes;
  `from_json` decodes the marker back to `Value::Bytes`. Bare
  strings continue to decode as `Value::Str`, so user strings that
  happen to be valid hex aren't reclassified. Malformed marker
  objects (odd-length hex, non-hex chars, extra keys) fall through
  to the `Record` decode. (#117, closes item 5 of #115)
- **`std.kv` registry is now LRU-bounded.** Capped at 256 open
  handles with FIFO eviction; long-running programs that opened
  many short-lived stores no longer leak `sled::Db` instances. Any
  `kv.{get,put,delete,contains,list_prefix}` touches the LRU
  order; `kv.open` past the cap drops the least-recently-used
  entry. Evicted handles return the standard "closed or unknown
  Kv handle" error on subsequent ops. (#119, closes item 3 of
  #115)
- **`std.process` registry now uses per-handle locks + bounded
  GC.** Each `ProcessState` is wrapped in `Arc<Mutex<…>>`, so the
  global registry mutex is held only briefly during dispatch
  lookup; reads / waits on different handles no longer block each
  other. Capped at 256 entries with LRU eviction.
  `process.wait` removes its entry on completion since the handle
  is terminal once the child exits — calling
  `read_stdout_line(h)` after `wait(h)` now returns the closed-
  handle error rather than draining buffered output. (#120,
  closes item 2 of #115)
- **`bench/REPORT.md` regeneration is now opt-in.** The
  `agent_sandbox_benchmark` test wrote the report as a side
  effect on every `cargo test --workspace`, so the file diff
  bled into unrelated PR branches. Gate the write behind
  `BENCH_WRITE_REPORT=1`; the test still runs and assertions
  still execute, only the file emission is opt-in. Regenerate
  with `BENCH_WRITE_REPORT=1 cargo test -p lex-cli --test
  agent_sandbox_bench`. (#121)
- **Diamond imports collapse to one nominal identity per file.**
  The loader's mangling key is now the canonical filesystem path
  (`<stem>_<8hex of sha256>`) rather than the alias chain, and the
  loader dedupes second loads of the same path. The natural
  `types/ + behavior/ + runner/` layout — where two siblings each
  import the same `models.lex` — now works: `Report` resolved
  through `scorer.m` and `verdict.m` is the same nominal type. The
  entry file is special-cased to an empty prefix so
  `lex run main.lex process` keeps working without users typing the
  hash. (#91, closes #88)
- **Anonymous record literals coerce to nominal record aliases at
  every position** — function argument, nested record field, list
  element, `let p :: T := { ... }`, constructor payload, pattern.
  Previously this only worked at function-return position, forcing
  POCs to write explicit `mk_*` constructor functions for every
  nominal record type. Two distinct nominal types with the same
  shape stay nominally distinct. (#86, closes #79)
- **Bare record patterns now match nominal record aliases.**
  `match v { { idea: pat, ... } => … }` works when `v` has a
  `type T = { ... }` annotation — mirror of the literal-coercion
  fix above. Unblocks the flat decision-table pattern (otherwise
  forced into a nested-match-per-axis tree). (#90, closes #89)
- **Trailing commas** are now allowed in every comma-separated
  list (fn params, call args, lambda params, type args, effects,
  function type params, constructor type payloads, constructor
  patterns, tuple patterns) — previously they were accepted in
  match arms / list / record literals only. (#84, closes #80)

### Fixed

- **`lex run` now decodes the `{"$variant": "Name", "args": [...]}`
  JSON convention** for variant arguments. Three crates each had
  their own copy of the JSON → `Value` decoder; only the CLI's was
  missing the variant-detection branch, so `lex run path.lex fn
  '{"$variant":"Red","args":[]}'` materialized as a `Value::Record`
  and tripped `TestVariant on non-variant` at the first match arm.
  Promoted the helper to `Value::from_json` in `lex-bytecode`
  (alongside the existing `to_json` it inverts); CLI, runtime
  (`json.parse`), and `lex serve` HTTP body all delegate. One
  source of truth for the JSON ↔ `Value` convention. (#94, closes
  #93)

### Dependencies

- logos `0.14` → `0.16`, tungstenite `0.21` → `0.29`, ureq
  `2.10` → `3.3`, sha2 `0.10` → `0.11`, thiserror `1` → `2`.
  Source changes for tungstenite (`Message::Text` now wraps
  `Utf8Bytes`) and ureq (full API rewrite — `Agent::config_builder`,
  `body_mut().read_to_string()`, `Error::StatusCode`). For ureq,
  `http_status_as_error(false)` preserves the prior
  `Err("status NNN: <body>")` shape. Features renamed to
  `rustls,platform-verifier`. (#75, #76)
- `actions/checkout@v4` → `@v6`, `actions/upload-artifact@v4`
  → `@v7`, `actions/download-artifact@v4` → `@v8`. (#84 build,
  bumped together because v8 download validates the
  upload side's content-type.)

### Documentation

- Badges row at the top of the README (CI, fuzz, tests, license,
  Rust MSRV).
- Worked `lex serve` example with `curl /v1/check` and `/v1/run`
  in the Quickstart.
- Multi-file project example in the Quickstart, demonstrating
  local imports.
- `lex check` examples updated to show the effect summary.

## [0.1.0] — 2026-05

The pre-launch baseline. Everything below is what shipped before
the changelog itself was started; entries are coarse-grained.

### Added

- **Agent-native VC, tier 1.** `lex branch` (`list` / `show` /
  `create` / `delete` / `use` / `current`), `lex store-merge`
  with three-way structural merge over branch heads using
  `fork_base` snapshots, `lex log` per-branch merge journal,
  `lex blame` per-fn stage history.
- **LLM-agnostic discovery.** Full [ACLI](https://github.com/alpibrusl/acli)
  compliance: `lex introspect` / `lex skill` / `lex version`,
  `--output text|json|table` on every subcommand, `--dry-run` on
  state-modifying ones, ACLI error envelopes with semantic exit
  codes. Auto-generated `.cli/` folder is committed.
- **AST tooling.** `lex audit` (structural search by effect / call
  / hostname / AST kind), `lex ast-diff` (with effect-change
  highlighting), `lex ast-merge` (three-way structural merge with
  JSON conflicts).
- **Persistent collections.** `std.map` and `std.set` with `Str`
  or `Int` keys (via `MapKey` so `Value` itself stays free of
  `Eq + Hash` constraints).
- **Effect polymorphism** on stdlib HOFs (`list.map`, `list.filter`,
  `list.fold`, `option.map`, `result.map`, `result.and_then`,
  `result.map_err`).
- **`lex agent-tool`.** Sandboxed runner for LLM-emitted tool
  bodies with effect declaration. Correctness ladder: `--examples`,
  `--spec`, `--diff-body`. Adversarial benchmark vs Python sandboxes.
- **`lex tool-registry serve`.** HTTP service to register Lex tools
  at runtime + invoke via `/tools/{id}/invoke`.
- **`lex spec`.** Randomized property checking + SMT-LIB export
  for external Z3.
- **Trace tree + replay + diff** (`lex run --trace`, `lex trace`,
  `lex replay`, `lex diff`).
- **Content-addressed store** (`lex publish`, `lex store list/get`)
  with stage lifecycle (Draft / Active / Deprecated / Tombstone).
- **Capability runtime + effect system** (`--allow-effects`,
  `--allow-fs-read PATH`, `--allow-fs-write PATH`,
  `--allow-net-host HOST`, `--budget`, `--max-steps`).
- **Type system**: HM inference, sized numerics, tensor shape
  solver, mutation analysis (Core), native matmul.
- **Stdlib MVP**: `std.str`, `std.int`, `std.float`, `std.bool`,
  `std.list`, `std.option`, `std.result`, `std.tuple`, `std.json`,
  `std.bytes`, `std.flow`, `std.math`, `std.io`, `std.net`,
  `std.chat`, `std.time`, `std.rand`.
- **Example apps**: weather REST API, multi-user WebSocket chat,
  CSV analytics, ML (linreg + logistic), webhook router, gateway
  service.
- **Conformance harness** with property tests.
- **Agent API server** (`lex serve` / `/v1/{parse,check,run,
  publish,patch,trace,replay,diff,stage}`).

### Hardening

- Parser recursion-depth gate (`MAX_DEPTH = 96`); closes a
  stack-overflow DoS the libFuzzer parser target found.
- VM call-stack depth gate (`MAX_CALL_DEPTH = 1024`); refuses with
  `VmError::CallStackOverflow` instead of unwinding the host.
- `SECURITY.md` threat model with deployment recommendations.
- `cargo fuzz` CI for parser + type checker (60 s/PR, 5 min nightly).

[Unreleased]: https://github.com/alpibrusl/lex-lang/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/alpibrusl/lex-lang/releases/tag/v0.1.0
