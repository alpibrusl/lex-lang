# Changelog

All notable changes to lex-lang. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
versioning follows [SemVer](https://semver.org/) (pre-1.0; minor
bumps may carry breaking changes when justified).

## [Unreleased]

### Added

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
