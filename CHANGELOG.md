# Changelog

All notable changes to lex-lang. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
versioning follows [SemVer](https://semver.org/) (pre-1.0; minor
bumps may carry breaking changes when justified).

## [Unreleased]

### Added

- **#382 (slice 3): `std.crypto` KDF primitives.** Key-derivation
  functions, all pure (no new effects), all returning
  `Result[Bytes, Str]` so caller-controlled inputs that violate the
  underlying primitive's contract surface as `Err` rather than
  panicking the VM:
  - **`crypto.pbkdf2_sha256(password, salt, iterations, len) ->
    Result[Bytes, Str]`** — RFC 8018 PBKDF2 with HMAC-SHA256. Backed
    by the `pbkdf2` crate. OWASP 2024 baseline is ≥ 600 000 iterations
    for password storage; older deployments pinning < 100 000 should
    rotate. Surface validations: iterations > 0, 0 < len ≤ 1 MiB,
    iterations fits in `u32`. Verified against the RFC 7914 §11
    test vector.
  - **`crypto.hkdf_sha256(ikm, salt, info, len) -> Result[Bytes, Str]`**
    — RFC 5869 extract+expand KDF over SHA-256. Use for deriving
    multiple keys from one high-entropy input (TLS / Noise key
    schedules, JWT-key rotation, per-session encryption keys). Empty
    salt is allowed (RFC substitutes a zero-string of `HashLen`).
    Output length capped at 255 × 32 = 8160 bytes by the primitive;
    a 1 MiB upper bound also applies. Verified against the RFC 5869
    Test Case 1 vector.
  - **`crypto.argon2id(password, salt, t_cost, m_cost, len) ->
    Result[Bytes, Str]`** — RFC 9106 Argon2id. The recommended choice
    for *new* password hashing. Backed by the `argon2` crate. The
    parallelism parameter `p` is pinned at 1 so hashes are comparable
    across machines; for variable `p`, build on top of the underlying
    crate via `lex-crypto`. Surface validations: `t_cost ≥ 1`,
    `m_cost ≥ Params::MIN_M_COST` (8), `len > 0`, salt ≥ 8 bytes (an
    Argon2 spec requirement, surfaced as `Err`).

  Closes #382 — together with slices 1 (convenience) and 2 (AEAD), the
  symmetric `std.crypto` surface specified in the issue is complete.
  Higher-level constructions (JWT, OAuth2 PKCE, signed cookies,
  password-storage wrappers with vetted defaults) land in the
  follow-up `lex-crypto` package (#383).

- **#382 (slice 2): `std.crypto` AEAD primitives.** Authenticated
  encryption with associated data, via:
  - **`crypto.aes_gcm_seal(key, nonce, aad, plaintext) -> Result[AeadResult, Str]`**
    and **`crypto.aes_gcm_open(key, nonce, aad, ciphertext, tag) -> Result[Bytes, Str]`**
    — AES-128-GCM or AES-256-GCM, picked from the supplied key length
    (16 or 32 bytes). Hardware-accelerated on most CPUs via AES-NI.
  - **`crypto.chacha20_poly1305_seal/open`** — same shape, fixed
    32-byte key, no hardware dependency. Preferred on constrained
    targets or when AES-NI isn't available.
  - **`AeadResult = { ciphertext :: Bytes, tag :: Bytes }`** — return
    shape for every seal op. Tag is split out from the ciphertext so
    callers don't have to know each algorithm's tag length (both
    happen to be 16 bytes today).
  - Both algorithms expect a 12-byte nonce. Seal returns
    `Result[AeadResult, Str]` (not bare `AeadResult`) so wrong
    key/nonce sizes surface as `Err` instead of panicking the VM.
    Open returns `Result[Bytes, Str]`; authentication failure (bad
    tag, modified ciphertext, modified AAD, wrong key, wrong nonce)
    is `Err`.

  KDF primitives (`pbkdf2_sha256`, `hkdf_sha256`, `argon2id`) land in
  slice 3, immediately above.

- **#382 (slice 1): `std.crypto` convenience adds.** Five new
  primitives on top of the existing `std.crypto` surface, no new
  effects required:
  - **`blake2b(bytes)`** — 64-byte digest, BLAKE2b-512. Faster than
    SHA-512 with the same security level. Backed by the `blake2` crate.
  - **`sha256_str(s)` / `sha512_str(s)`** — hash a `Str` directly and
    return the digest as a lowercase hex `Str`. Equivalent to
    `crypto.hex_encode(crypto.shaN(bytes_of_str(s)))` for the common
    case where the caller already has a string.
  - **`base64url_encode` / `base64url_decode`** — URL-safe base64
    (`-_` alphabet, no padding). Required for JWT segments, signed
    cookies, and any token traveling in a URL.
  - **`eq` / `eq_str`** — constant-time equality on `Bytes` and `Str`
    respectively. `eq` is the recommended spelling; `constant_time_eq`
    stays as an alias.
  - **`random_str_hex(n)`** — N random bytes rendered as 2N hex chars,
    gated by `[random]`. The canonical token-mint shape (session ids,
    OAuth `state`, CSRF tokens, request ids).

  AEAD primitives (`aes_gcm_seal/open`, `chacha20_poly1305_seal/open`)
  and KDFs (`pbkdf2_sha256`, `hkdf_sha256`, `argon2id`) are the next
  slices — each adds 2–3 new crypto crates and warrants its own
  focused PR.


### Added

- **#378: `std.time` extensions — `now_ms`, `now_str`, `mono_ns`.**
  Three new ops on the existing `time` module: `time.now_ms()` (Unix
  milliseconds, the natural resolution for request-latency and
  rate-limiter windows), `time.now_str()` (ISO-8601 / RFC 3339 in UTC,
  for `created_at` / `updated_at` timestamps and structured log
  lines), and `time.mono_ns()` (monotonic nanoseconds since process
  start, for duration measurement that's immune to NTP jitter). All
  three carry the existing `[time]` effect. `time.now`, `time.now_ms`
  and `time.now_str` all honor `LEX_TEST_NOW` (Unix seconds) for
  deterministic tests, lifting the seconds value to the right
  resolution per op; `time.mono_ns` deliberately doesn't pin since a
  fixed monotonic clock defeats its purpose. `time.now` is now also
  pinnable via `LEX_TEST_NOW`, closing a small consistency gap with
  `datetime.now`.

### Changed

- **#380: structured `SqlError` on the `Err` side of every `std.sql`
  Result.** Replaces `Err(Str)` with `Err(SqlError { message :: Str,
  code :: Option[Str], detail :: Option[Str] })`. `code` carries the
  symbolic SQLite extended-result-code name (`SQLITE_CONSTRAINT_UNIQUE`,
  `SQLITE_BUSY`, …) or the 5-character Postgres SQLSTATE (`23505`,
  `40P01`, …) so dialect-aware retry / conflict handling can dispatch
  without parsing error messages. **Breaking change** — callers
  pattern-matching `Err(s)` where `s` was treated as `Str` must access
  `e.message` instead. Affects `sql.open`, `exec`, `query`,
  `query_iter`, `begin`, `commit`, `rollback`, `exec_tx`, `query_tx`.

### Added

- **#375: streaming HTTP response bodies for `net.serve_fn`.** The
  registered `Response` alias's `body` field changes from `Str` to a
  new `ResponseBody` union:
  ```
  type ResponseBody =
      BodyStr(Str)
    | BodyStream(Iter[Str])
    | BodyBytes(Iter[List[Int]])
  ```
  `BodyStr` is the existing eager-string path. `BodyStream` and
  `BodyBytes` drain an `Iter[T]` and emit the body under
  `Transfer-Encoding: chunked` (no `Content-Length`). With `Iter[T]`
  now lazy via `iter.unfold` (#376), the drain pulls from a
  closure-backed producer one item at a time — true SSE / large-file
  streaming. The runtime keeps an escape hatch that accepts a bare
  `Str` body field for handlers that declare their own structural
  `Response` type instead of the registered alias, so existing
  example apps (analytics_app, gateway_app, etc.) keep working
  without changes. New `examples/streaming_app.lex` showcase serves
  three routes — `/`, `/sse`, `/blob` — covering all three variants.
  Wire-level integration test in
  `crates/lex-runtime/tests/net_streaming.rs` confirms
  `Transfer-Encoding: chunked` is set and the decoded body matches
  the joined iter items.

## [0.9.1] — 2026-05-13

### Added

- **#369: signature-level `examples { ... }` block on `FnDecl`.** A pure
  function can now carry an optional block between its return type and
  body listing input/output examples that fold into the canonical AST. Two
  signatures with different example sets hash to different SigIds —
  examples are part of the contract, not an external test file. Slice 1
  (PR #370) added the AST + parser + canonical-hash compat (regression
  test pins `FnDecl { examples: vec![], .. }` to its pre-#369 hash) + the
  type-level checks: argument arity, arg types against parameter types,
  expected type against return type, and pure-only enforcement
  (`ExamplesOnEffectfulFn` rule tag). Slice 2 (PR #373) added behavioral
  evaluation: every example case runs through the bytecode VM at
  `lex check` time and is compared to the declared expected value;
  mismatches surface as `ExampleMismatch` with stable rule_tag
  `example-mismatch` and a `suggested_transform` payload for the repair
  flow. The `factorial`, `parse_int`, `double_input`, and `area` examples
  carry working `examples` blocks as showcases. Closes #369.

- **#363: record row spreads — `{ ...TypeName }` in type expressions.**
  Type expressions accept the spread form so a record type can be defined
  as the union of fields from one or more other record types. Unblocks
  type-safe JOIN results in lex-orm — `Merge[A, B] = { ...A, ...B }` now
  has a direct surface form. Closes #363.

- **#364: `Iter[T]` lazy positional iterator stdlib.** New `std.iter`
  module with `from_list / next / is_empty / count / take / skip /
  to_list / map / filter / fold`. Backed at runtime by a
  `(List[T], Int)` tuple with the Int as the cursor; all operations are
  compiler-inlined as bytecode so no runtime effect dispatch is needed.
  `Iter[T]` registered as an opaque type. Enables short-circuiting,
  one-row-at-a-time consumption patterns over `std.list` and (in
  combination with #362) over `std.sql` result sets. Closes #364.

- **#362: Postgres support in `std.sql` + typed `SqlParam` ADT +
  transactions + row decoders.** `sql.open` now dispatches on URL prefix:
  `postgres://` / `postgresql://` connect via the sync `postgres` crate,
  anything else opens SQLite as before. Parameters use the new
  `SqlParam = PStr(Str) | PInt(Int) | PFloat(Float) | PBool(Bool) | PNull`
  union instead of the v1 `List[Str]` workaround. New transaction surface:
  `sql.begin(db) -> SqlTx`, `sql.commit / rollback(tx)`, and
  `sql.exec_tx / query_tx(tx, ...)`. New row decoders
  `sql.get_str / get_int / get_float / get_bool :: T, Str -> Option[X]`.
  Effect annotations: `[sql]` for query/exec/begin/commit/rollback;
  `[sql, fs_write]` for `open` (SQLite creates the file on first open).
  Closes #362.

- **#365: `lex fmt`, `lex init`, `lex ci` + `lex pkg install`.**
  `lex init [<dir>]` scaffolds a new project with `lex.toml`,
  `src/main.lex`, `tests/test_main.lex`, and a `.github/workflows/lex.yml`.
  `lex fmt [--check] <file|dir>...` formats `.lex` files via the
  canonical pretty-printer; `--check` exits 1 if any file needs
  formatting (suitable for CI). `lex ci [--no-fmt] [--src <d>]
  [--tests <d>]` runs the full local CI pipeline: `pkg install →
  check --strict → fmt --check → test`. `lex pkg install` resolves and
  installs all dependencies from `lex.toml`, cloning git dependencies
  into the cache on first use. Closes #365.

- **#347 A2 phase 3: bytecode stack-depth verifier as third
  `--strict` check.** `lex check --strict` now runs a bytecode pass that
  walks each function's instruction stream, tracks abstract stack depth
  through opcodes and branches, and warns when two paths into the same
  program counter carry different depths — catching `PConstructor` stack
  leaks that the type checker cannot see. Surfaces as a `STACK_DEPTH`
  lint warning (exit 1 in CI, non-fatal otherwise). Closes #347 (A2 phase).

## [0.9.0] — 2026-05-12

### Added

- **#347 A2: `lex check --strict`** — runs two AST lint passes after a
  clean type-check. `STR_CMP` warns when an ordering operator (`<`, `<=`,
  `>`, `>=`) is applied to a `Str` literal (lexicographic ordering is
  rarely the intent). `SHADOW_FN` warns when a function parameter, `let`
  binding, or lambda parameter shadows a top-level function by the same
  name. Lint warnings are emitted as JSON and cause exit 1 so CI can
  enforce them. Closes #347 (item A2).

- **#349: `lex test` subcommand** — walks `tests/test_*.lex`, calls
  `run_all()` in each file under a permissive policy, and exits non-zero
  on any failure. Replaces per-project shell loops; gives CI a stable
  entry point. Closes #349.

- **#351: `lex repl --load <file>`** — pre-loads one or more `.lex` source
  files into the REPL session before the prompt appears. Repeatable.
  Closes #351.

- **#352: `docs/AGENT.md`** — cold-start guide for AI agents covering the
  iteration loop, `lex check` JSON error envelope, effect system, stdlib
  surface, and known sharp edges. Closes #352.

- **#354: `net.serve_fn`** — effect-polymorphic HTTP server variant that
  accepts a first-class closure `(Request) -> [Eff] Response` instead of
  a handler name string. The handler's effect row propagates to the call
  site. Closes #354.

- **#355: Response headers** — `Request` now carries an incoming
  `headers :: Map[Str, Str]` field (keys lowercased). `Response` gains a
  `headers :: Map[Str, Str]` field; the runtime forwards those headers
  through `tiny_http`. Closes #355.

- **#358: Import path canonicalization** — `resolve_import` now calls
  `.canonicalize()` after resolving a relative path, so `../../shared/foo`
  and `../other/../shared/foo` hash to the same key in the loader's dedup
  maps. Prevents duplicate loads and mismatched mangling prefixes in
  diamond-import graphs. Closes #358.

- **#359: `net.serve_ws_fn` + `WsConn`/`WsMessage`/`WsAction` types** —
  three new global types model the WebSocket primitive surface:
  `WsConn = { id :: Str, path :: Str, subprotocol :: Str }`,
  `WsMessage = WsText(Str) | WsBinary(List[Int]) | WsPing | WsClose`,
  `WsAction = WsSend(Str) | WsSendBinary(List[Int]) | WsNoOp`.
  `net.serve_ws_fn :: (Int, Str, (WsConn, WsMessage) -> [Eff] WsAction) -> [net, Eff] Unit`
  accepts a typed closure handler with effect propagation. The existing
  string-based `net.serve_ws` is unchanged. Closes #359.

- **`lex.toml` package manifest (Phase 1)** — projects declare
  dependencies in a `lex.toml` file at the project root. Import paths of
  the form `"pkg-name/module"` are resolved against the nearest manifest
  found by walking up the directory tree. Supported dependency kinds:
  `path = "../local/dir"` and `git = "https://..."` (clones to
  `~/.lex/packages/` on first use; override with `$LEX_PACKAGES_DIR`).
  Module search: `{pkg_root}/src/{module}.lex`, then
  `{pkg_root}/{module}.lex`. New `lex pkg` subcommands: `init`, `add
  --path`, `add --git`, `list`.

### Fixed

- **#348: VM panic messages now include function names.** Panics such as
  "step limit exceeded" and "ran past end of code" previously showed a raw
  `fn_id` integer. They now include the declared function name (e.g.
  `step limit exceeded in 'tally_escapes'`). `VmError::UnknownFunction`
  also carries the name string. Closes #348.

- **#350: `LEX_TEST_NOW` pins `datetime.now()` in tests.** Setting
  `LEX_TEST_NOW=<unix_seconds>` makes `datetime.now()` return a fixed
  nanosecond timestamp derived from that value. Effect tracking is
  unchanged (`[time]` is still required). Closes #350.

## [0.8.2] — 2026-05-12

### Fixed

- **#345: type-alias unfold now reaches closure params and return types.**
  `unify_coerce_inner` previously fell through to plain `unify` for
  `Function` types, so a closure annotated `(Errors, Errors) -> Errors`
  failed to unify with `list.fold`'s expected `(List[?n], ?m) -> List[?n]`
  even when `Errors = List[Error]`. Adding a recursive `Function` case
  (mirroring the existing `Tuple` and `Con-Con` cases) closes the gap for
  all polymorphic stdlib HOFs. Closes #345.

- **#332: `Str < Str` / `Str <= Str` / `Str > Str` / `Str >= Str` no longer
  crash at runtime.** The type checker already admitted string comparisons; the
  VM's `bin_ord` helper now handles `Value::Str` operands via lexicographic
  order. Closes #332.

### Added

- **#334: `list.cons :: T, List[T] -> List[T]`** — O(1)-amortised prepend.
  Enables the idiomatic functional builder loop: cons elements in reverse, call
  `list.reverse` once at the end. `list.reverse` was already present; `cons`
  was the missing half. Closes #334.

- **#331: `datetime.before`, `datetime.after`, `datetime.compare` + new
  `duration` module.** `datetime.before/after :: Instant, Instant -> Bool` and
  `datetime.compare :: Instant, Instant -> Int` (-1/0/+1) expose typed
  Instant ordering without falling back to ISO 8601 string comparison. New
  `import "std.duration" as duration` module exposes `duration.seconds ::
  Duration -> Int` (truncates nanoseconds to whole seconds), complementing the
  existing `datetime.duration_seconds` constructor. Closes #331.

## [0.8.0] — 2026-05-12

### Added

- **#304 phase 4: `RepairHint` surface in `lex-lsp`.** Closes the
  loop from typecheck failure → durable `RepairHint` attestation
  (#281) → editor lightbulb. When the LSP is launched with
  `LEX_STORE=<store-path>`, every fn in the open file whose
  `stage_id` has an active `RepairHint` attestation surfaces as a
  QuickFix code action titled
  *"Lex: repair hint for `<fn>` (<rule_tag>) — <kind_hint>"*.
  The `data` payload carries `failed_op_id`, `stage_id`, the
  structured errors, the `suggested_transform`, and the
  `attestation_id`, so a client extension (or phase 4b) can
  invoke `lex repair --apply` and refresh the buffer. Hints are
  resolved lazily per code-action request (no upfront index
  build); the store is opened with `Store::open` on each request
  and falls through silently when `LEX_STORE` is unset or the
  path doesn't open — editors without a configured store see the
  same surface as before. Coverage: 3 new lib unit tests
  (round-trip through a real temp store; missing store → empty;
  unparseable source → empty) + 1 e2e test through the protocol
  with a real store that has a planted RepairHint.

- **#304 phase 3b: applying `Inline let` refactor in `lex-lsp`.**
  First typed-transform (`InlineLet` from #280) that lands a real
  `WorkspaceEdit` instead of just a hint. The `textDocument/codeAction`
  handler walks the file's canonical AST; for every fn whose body
  is a top-level `let` and whose declaration falls inside the
  requesting range, it emits a `Refactor.Inline` code action whose
  `edit` is a full-document `TextEdit` replacing the source with
  the canonical re-print after `inline_let`. Selecting the
  lightbulb applies the refactor inline, no CLI round-trip. The
  other three #280 transforms (`RenameLocal`, `ReplaceMatchArm`,
  `ExtractFunction`) still need cursor-to-NodeId mapping that's
  queued for a follow-up — top-level let is the case where the
  target NodeId derives from fn structure alone
  (`n_0.{params + 1}`). Coverage: 4 lib unit tests on
  `inline_let_actions` + 2 e2e tests round-tripping the protocol
  (action surfaces with a real `documentChanges` `edit`;
  no-top-level-let → no action).

- **#304 phase 3a: code-action surface in `lex-lsp`.** Editors
  now show a lightbulb on every type-error diagnostic that has a
  static `suggested_transform` (#306 slice 3): the action title is
  the suggestion's `summary` plus the typed-transform `kind_hint`
  (e.g. *"Lex: Replace the offending match arm... (ReplaceMatchArm)"*).
  The full suggestion JSON is attached as `data` so a client
  extension can pipe it to `lex repair --apply --transform '<json>'`.
  Computing a real `WorkspaceEdit` (so the edit applies in the
  editor without a CLI round-trip) is queued for phase 3b — that
  needs cursor-to-NodeId mapping plus AST-roundtrip pretty-printing.
  Coverage: 2 new lib unit tests on `code_actions_for_diagnostics`,
  2 new e2e tests through the protocol (action surfaces with the
  right title / kind / data; no diagnostics → no actions).

- **#304 phase 2a: hover / definition / completion in `lex-lsp`.**
  Builds on phase 1's read-only diagnostics. Editors get three
  new request handlers:
  - `textDocument/hover` — renders the function signature plus
    declared effects and budget at the cursor as Markdown.
  - `textDocument/definition` — jumps to the `fn` keyword of the
    declaration when invoked on a call site in the same file.
    Cross-file definition is queued for phase 2b.
  - `textDocument/completion` — proposes in-scope fn names and
    import aliases. Trigger character `.` is registered so phase
    2b can wire up `io.<TAB>` / `list.<TAB>` stdlib-member
    completion.

  Coverage: 6 new lib unit tests on `word_at` / `analyze_source` /
  `hover_at` / `definition_at` / `completions`, plus 3 new e2e
  tests spawning the binary and round-tripping each request
  through the protocol. Workspace test count: 1389 passing.

- **#304 phase 1: `lex-lsp` Language Server.** Editors that speak
  LSP (VS Code, Cursor, Continue, Zed, JetBrains AI) now light up
  Lex files with inline red squiggles for type errors instead of
  needing a separate `lex check` pass. New `lex-lsp` crate ships
  the JSON-RPC loop over stdin/stdout (`lsp-server` +
  `lsp-types`); `initialize` / `initialized` / `shutdown`
  lifecycle plus `textDocument/didOpen` / `didChange` / `didSave`
  / `didClose` with full-document sync. Every type error becomes
  a `Diagnostic` carrying severity ERROR, the stable
  `rule_tag` (#306 slice 2) as `code`, source `"lex"`, and a
  `data` payload with `rule_explanation`,
  `suggested_transform` (#306 slice 3), and the `at_node` —
  phase-3 code-action providers will read the suggestion from
  there. Phase 2 (hover / definition / completion), phase 3
  (typed-transform code actions), and phase 4 (RepairHint
  surface) are queued as follow-up slices. Coverage: 5 lib unit
  tests on the diagnostic-translation path + 2 e2e tests
  spawning the compiled binary and round-tripping the protocol.

- **#305 slice 3: `Stream[T]` + `agent.cloud_stream`.** Closes
  #305. Lex agents can now consume LLM completions chunk-by-chunk
  instead of blocking on the full response. New nominal `Stream[T]`
  opaque type registered in the type checker; new `stream` builtins
  module with `stream.next(Stream[T]) -> [stream] Option[T]` and
  `stream.collect(Stream[T]) -> [stream] List[T]`; new
  `agent.cloud_stream(prompt) -> [llm_cloud] Result[Stream[Str], Str]`
  producer. The runtime represents a Stream value as the opaque
  variant `__StreamHandle(handle_id)` and holds an
  `Arc<Mutex<HashMap<handle_id, Box<dyn Iterator + Send>>>>` on
  `DefaultHandler`; the registry is shared across par_map workers
  via slice 2's `spawn_for_worker`. The fixture path
  (`LEX_LLM_STREAM_FIXTURE='chunk1|chunk2|…'`) is the test hook;
  live HTTP chunked-response support is deferred. Coverage: 5
  conformance tests in `crates/lex-runtime/tests/stream_basic.rs`
  pin laziness (next + next + collect splits the stream at the
  right boundary), the `None` past-end contract, and effect-gate
  refusal. Tests serialise on a per-file mutex so cargo's parallel
  scheduler can't race the process-global fixture env var.


- **#307: cost-aware planner (`lex plan`).** Closes the loop opened
  by `[budget(N)]` declarations and the session-budget gate (#292):
  given a `goal` function, enumerate every linear call chain from
  `goal` to a leaf, sum declared budget along each chain, and rank
  cheapest-first. Each path carries the union of declared effects so
  the agent can also gate by policy. The output is advisory —
  paths are returned with a `fits` flag against the effective cap
  (`min(--max-cost, session-remaining)`); the agent (or downstream
  policy) chooses which path to apply.
  - New `lex_store::planner` module with `Store::plan(branch,
    goal, max_cost, session_id) -> Plan`.
  - Recursive self-calls are detected via a visited-set and
    budgeted once — no infinite-loop expansion.
  - When `session_id` is supplied, the planner consults
    `Store::session_budget` and merges remaining with `--max-cost`.
  - New `lex plan --goal <fn> [--max-cost N] [--intent <id>]
    [--branch B] [--store DIR]` CLI subcommand.
  - 6 conformance tests in `crates/lex-store/tests/planner.rs`
    pin the contract (linear chain budget sum, max-cost
    would-exceed marking, branching → multiple sorted paths,
    recursive self-call budgeted once, effect-set union, unknown
    goal yields empty paths).

- **#305 slice 2: per-thread effect handler split for
  `list.par_map`.** Slice 1 ran each worker with `DenyAllEffects`,
  so effectful closures (MCP calls, LLM invocations, io.print)
  failed at runtime — the actual agent use case for par_map was
  blocked. Slice 2 adds `EffectHandler::spawn_for_worker(&self) ->
  Option<Box<dyn EffectHandler + Send>>` (default `None` keeps
  slice-1 behavior). `DefaultHandler` implements it by yielding a
  fresh `DefaultHandler` per worker that **shares the parent's
  budget pool** via `Arc<AtomicU64>` (so parallel work can't escape
  the run-wide ceiling) while keeping `mcp_clients` and `sink`
  per-worker to avoid mutex-serializing the dispatch path.
  `chat_registry` and `program` (both `Arc`) are cloned across
  workers. The `ParallelMap` op now consults the parent's
  `spawn_for_worker` and pre-builds one handler per worker on the
  main thread before `std::thread::scope`, so the worker owns its
  handler outright with no shared mutable state. Two new
  conformance tests pin the contract: an effectful par_map under
  `DefaultHandler` now succeeds; a par_map whose total budget
  cost exceeds the parent ceiling is rejected by the shared pool.

- **#305 slice 1: `list.par_map` with OS-thread parallelism.**
  Lex's runtime was fully synchronous; multiple concurrent effect
  calls or CPU-bound work from a single program had no way to be
  expressed in parallel. This slice ships `list.par_map(xs, f)` —
  the bytecode compiler intercepts the call (mirroring `list.map`'s
  inline emission) and emits a new `Op::ParallelMap`. At runtime
  the VM partitions `xs` into round-robin buckets across N worker
  threads (capped by `LEX_PAR_MAX_CONCURRENCY`; default = available
  CPU cores, max 64), spawns each on `std::thread::scope`, and
  reassembles results in input order. The type signature mirrors
  `list.map`'s effect-polymorphic shape so closures with declared
  effects still type-check.

  Slice 1 limitation: worker threads run with `DenyAllEffects`, so
  effectful closures fail at runtime with `VmError::Effect`. The
  per-thread effect-handler split (so closures can call MCP tools
  / LLMs in parallel) is queued as slice 2. Slice 3 ships
  streaming primitives (`Stream[T]` + `llm.cloud_stream`).

  Coverage: 5 conformance tests in
  `crates/lex-runtime/tests/list_par_map.rs` (input-order
  preservation, empty-list, cap=1, cap < N, effectful-closure
  rejection). A 6th wall-clock-speedup test is `#[ignore]` because
  sandboxed CI runners commonly serialize OS threads even when
  `available_parallelism()` reports multiple cores; run it under
  real multi-core CI with `--ignored --test-threads=1`.

- **#306 slice 3: auto-populated `suggested_transform` on
  `RepairHint`.** Closes #306. When
  `Store::apply_operation_checked` rejects an op for a `TypeError`,
  the gate now consults a static (rule_tag → likely_transform)
  table and pre-populates the `RepairHint` attestation's
  `suggested_transform` payload. Seven rule_tags ship with a
  static hint: `type-mismatch` → `ReplaceMatchArm`,
  `unknown-identifier` → `RenameLocal`, `non-exhaustive-match` →
  `ReplaceMatchArm`, `effect-not-declared` → `ChangeEffectSig`,
  `arity-mismatch` → `ModifyBody`, `unknown-field` →
  `ModifyBody`, `ambiguous-type` → `ModifyBody`. Each suggestion
  includes a one-sentence `summary` and longer `details` prose
  suitable for an LLM repair prompt. The LLM-driven `lex repair
  --apply` path still works for rules without a static suggestion
  and can overwrite a static suggestion with a higher-quality
  one. New `lex_types::suggested_transform_for(rule_tag)` is
  available for any tooling that wants to render the same hint
  inline (LSP code-actions, repair-flow prompts).

- **#306 slice 2: rule-tagged type errors + `lex docs --rules`.**
  Every `TypeError` variant gains a stable `rule_tag(&self) -> &'static str`
  (kebab-case identifier: `"type-mismatch"`, `"unknown-identifier"`,
  `"effect-not-declared"`, …) and a `rule_explanation(&self) -> &'static str`
  (plain-language description of what the rule enforces, suitable to
  inline in an LLM repair prompt). LLM repair flows that reference the
  `rule_tag` get measurably better repair attempts because the model can
  cross-reference the rule across many prior examples. The `PositionedError`
  JSON envelope now carries both fields alongside the existing `kind` and
  `position`. New `lex docs --rules` subcommand enumerates the full
  catalog (12 rules for 0.7.x); the JSON envelope is keyed by
  `rules: [{ rule_tag, rule_explanation }]` so LSP servers and other
  agent tooling can ingest it directly.

- **#306 slice 1: position-aware type errors.** LLM repair flows
  measurably need `file:line:col` on type errors, not bare NodeIds.
  The `lex_types` crate gains a `Position { file, line, col }`
  struct, a `PositionedError` wrapper that carries
  `TypeError + Option<Position>`, and a new
  `check_program_with_positions(stages, &BTreeMap<fn_name, Position>)`
  entry point. The parser side gains
  `parse_source_with_positions` which yields each `fn`
  declaration's byte-offset start position; the `byte_to_line_col`
  helper translates that to a 1-based `(line, col)`. The `lex
  check` CLI wires the two together and now emits `position` on
  every type error in its JSON envelope. Granularity is function-
  level for this slice — every error from a given `fn` is stamped
  with that `fn`'s position; slice 1.5 will plumb per-expression
  spans through canonicalize so deep-body errors point at the
  exact sub-expression. The bare `check_program` API keeps its
  old `Vec<TypeError>` shape so existing callers and tests are
  unaffected.

- **#308: `+` operator overloaded for `Str` operands.** Lex already
  shipped `std.str.concat` and the `Op::StrConcat` bytecode, but
  the surface-language `+` only accepted `Int | Float`, forcing
  agents to reach for stdlib calls for the most common operation
  in code-generation paths. Now `a + b` works uniformly across
  `Int + Int`, `Float + Float`, and `Str + Str`; the type checker
  admits all three for `+` while keeping `-/*/%` numeric-only, and
  the VM dispatches `Op::NumAdd` to string concatenation when both
  operands resolve to `Str`. No coercion: `Str + Int` still errors
  with a clear `TypeMismatch` diagnostic. Closes the first of the
  five agent-UX gaps filed as #304–#308.

## [0.7.1] — 2026-05-11

Patch release containing a single bug fix that surfaced in CI
after 0.7.0 tagged.

### Fixed

- **CAS retry race in `apply_operation` for empty-parents ops**
  (#262 follow-up). `cas_retry_advance`'s attempt-1
  "honor caller's exact op" semantics surfaced `StaleParent`
  on a legitimate race: when a sibling writer landed between
  the read of `head_op` and the local persist, the caller's
  `parents = []` op was rejected by `lex_vcs::apply` instead of
  retrying. CI hit this on the multi-writer stress test. Fix:
  on attempt 1, rebuild the op against the just-read head when
  the caller's `parents` is empty — that's the "I don't care,
  chain off whatever the current head is" intent. Non-empty
  parents on attempt 1 still surface `StaleParent` so the
  explicit-bogus-parent test (`apply_operation_with_stale_parent_errors`)
  keeps its contract. New regression test
  `empty_parents_op_chains_off_existing_head` covers the path
  single-threaded so the race is deterministically reproducible.

## [0.7.0] — 2026-05-11

The post-0.6.0 strategic-amplifier wave. Five issues land in
one release — the four amplifiers identified in the 0.5.0
review plus the closure of #281's LLM-driven repair path:

- **#281 closed repair loop, slices 2a + 2b** — `lex repair
  --apply --transform '<json>'` for typed-transform execution
  with a `RepairAttempt` audit trail, and `lex repair --apply`
  (no `--transform`) for LLM-driven transform generation. The
  `LEX_REPAIR_LLM_FIXTURE` env var short-circuits the LLM call
  for tests; production calls `lex_runtime::llm::cloud_complete`.
- **#292 per-session budget enforcement** — `Store::session_budget`
  ledger (slice 1, shipped in 0.6.0 mid-cycle), `policy.session_budgets`
  schema with `default_cap` + per-session `overrides` (slice 2),
  and an `apply_operation_checked` budget gate with
  `StoreError::BudgetExceeded` mapped to HTTP 503 + `Retry-After: 0`
  (slice 3).
- **#293 positive `ProducerTrust`** — `AttestationKind::ProducerTrust
  { tool_id, score_thousandths, ... }` complementing #248's
  `ProducerBlock`; `policy.required_attestations[].skip_if_producer_trust_thousandths_above`
  waiver hook; `TrustWaived` audit attestation;
  `lex producer-trust recompute` CLI. Hard-vetoes blocked producers.
- **#294 multi-agent Candidate/Promote ops** —
  `OperationKind::Candidate` proposes a stage without advancing the
  branch (no CAS contention between concurrent agents);
  `OperationKind::Promote` picks a winner and lists every other
  live candidate in `supersedes`. `lex stage candidates <sig_id>`
  and `lex stage promote-candidate <op_id>` CLI.

Minor bump because every data-layer change is additive: new
`OperationKind` variants (`Candidate`, `Promote`) and new
`AttestationKind` variants (`ProducerTrust`, `TrustWaived`)
are append-only enum extensions; new `policy.json` fields
(`session_budgets`, `skip_if_producer_trust_thousandths_above`)
all use `skip_if_none` so pre-0.7.0 policy files load
unchanged; new CLI subcommands add surface without changing
existing commands.

One behavior change worth calling out: `apply_operation_checked`
now consults the per-session budget cap after typecheck passes
and may surface `StoreError::BudgetExceeded`. Callers that
weren't pattern-matching on `StoreError` exhaustively (i.e.
used a wildcard arm) are unaffected. Stores with no
`policy.session_budgets` (the pre-0.7.0 shape) keep their
current behavior — the gate is no-op when no cap is set.

### Added — agent-coordination (#294)

- **`OperationKind::Candidate { sig_id, stage_id }`** — proposes
  a stage for `sig_id` without advancing the branch head. Produces
  `StageTransition::ImportOnly`. Multiple agents can land
  Candidates on the same sig concurrently with no contention.
  The `Operation`'s `intent_id` distinguishes proposals by
  author.
- **`OperationKind::Promote { sig_id, winner_candidate,
  winner_stage_id, supersedes, from_stage_id, from_budget,
  to_budget }`** — promotes one `Candidate` as the new branch
  head for its sig. Lists every other live `Candidate` in
  `supersedes` so the op log explicitly records the bake-off
  shape. Produces `Replace` (or `Create` when `from_stage_id`
  is `None`) and runs through `apply_operation_checked` so the
  re-typecheck and existing gates apply.
- **`Store::propose_candidate(branch, new_stage, intent_id)`** —
  publishes the stage, emits a `Candidate` op tagged with the
  intent. Doesn't run the gate.
- **`Store::list_candidates(sig_id)`** — returns every live
  `Candidate` (not yet referenced as winner or in `supersedes`
  by any `Promote`). Sorted by op_id for deterministic output.
- **`Store::promote_candidate(branch, candidate_op_id)`** —
  emits a `Promote` op advancing the head. Refuses non-
  Candidate ops with `StoreError::InvalidTransition`. Re-
  typechecks the candidate program through
  `apply_operation_checked` so ill-composed promotions surface
  as `TypeError`.
- **`lex stage candidates <sig_id>`** and **`lex stage
  promote-candidate <op_id> [--branch B]`** CLI surfaces.
- **`CandidateInfo`** public type re-exported from `lex-store`
  for `lex stage candidates` JSON envelope.
- 7 conformance tests covering: propose doesn't advance head;
  list returns every proposal; promote advances + supersedes
  others; typecheck on promote; unknown op errors;
  non-Candidate op rejected; cross-sig candidates isolated.

### Added — agent-trust (#293)

- **`AttestationKind::ProducerTrust { tool_id,
  score_thousandths, evidence, granted_by }`** — positive trust
  signal complementing #248's `ProducerBlock`. Score is stored as
  `u32` thousandths (`0..=1000`, representing 0.0..1.0) because
  `AttestationKind` derives `Eq` for content-addressed hashing,
  which `f64` doesn't implement. Derivation method: `passed /
  (passed + failed + inconclusive)` over the producer's most-
  recent `window` attestations.
- **`AttestationKind::TrustWaived { producer, score_thousandths,
  threshold_thousandths, kind_tag }`** — audit signal recording
  that the `required_attestations` gate skipped a rule because a
  trusted producer's score exceeded the rule's threshold.
- **`policy.required_attestations[].skip_if_producer_trust_thousandths_above`**
  (`Option<u32>`, default `None`). When set, the gate consults
  the maximum live trust score across all non-blocked producers;
  if it exceeds the threshold, the rule is waived and a
  `TrustWaived` attestation lands for the audit trail.
- **`Store::recompute_producer_trust(tool_id, window, granted_by)`**
  walks the attestation log filtered by `produced_by.tool`,
  scores `passed/total` over the last `window` records, and
  emits a fresh `ProducerTrust` attestation. Refuses to grant
  trust to a tool with an active `ProducerBlock` (hard veto).
  Self-referential trust attestations are excluded from the
  evidence corpus so re-running the recompute is stable.
- **`lex producer-trust recompute --tool <id> [--window N]
  [--granted-by ACTOR] [--store DIR]`** CLI runs the recompute
  and emits the resulting attestation_id. JSON envelope reports
  `{tool, window, granted_by, attestation_id, ok}`.
- 11 conformance tests (8 store-gate + 3 CLI).

### Added — agent-feedback (#281, slice 2b)

- **`lex repair <op_id> --apply`** (no `--transform`) now invokes
  the configured LLM to generate a typed transform from the
  failed-op context, then dispatches via slice-2a's apply path.
  Closes the full agent feedback loop: an ill-typed op produces
  a structured `RepairHint`; `lex repair --apply` reads the hint,
  asks the LLM for a single typed transform across the four #280
  variants, and lands it (or records the failed attempt).
- **Structured prompt** with inlined JSON schemas for each
  transform, the branch-head stage AST (the one transforms
  operate against), the candidate stage that didn't typecheck
  (informative), and the type errors. The model is instructed
  to respond with ONLY the transform JSON — no prose, no
  markdown fences.
- **Test infrastructure**: `LEX_REPAIR_LLM_FIXTURE=<path>` env
  var short-circuits the live LLM call by reading the response
  from that file. Lets the CLI subprocess tests assert
  end-to-end behavior without any network dependency. Production
  callers (env var unset) hit `lex_runtime::llm::cloud_complete`.
- **Graceful degradation**: a malformed LLM response (not JSON,
  or missing `kind`) is recorded as a `RepairAttempt` failure
  rather than propagated as exit-non-zero. The command itself
  always exits 0; outcome lives in the envelope and the
  attestation log. Same shape as slice 2a's ill-typed-transform
  path.
- 4 conformance tests in `crates/lex-cli/tests/repair_llm.rs`
  driving the fixture path (well-typed lands; malformed JSON
  records failure; unknown kind records failure; ill-typed
  transform records failure).
- The slice-2a "requires `--transform`" error is gone — the LLM
  path is now the default behavior of `--apply`.

With slices 1 + 2a + 2b shipped, **#281 is complete** modulo the
explicitly-out-of-scope `--max-iters` looping (single-shot
today).

### Added — agent-safety (#292, slices 2 + 3)

- **`policy.session_budgets` schema** with `default_cap` and
  per-session `overrides` (#292 slice 2). An override entry of
  `Some(n)` sets a session-specific cap; an entry with explicit
  `null` means unbounded for that session (escape hatch for human
  interventions). Absent `session_budgets` keeps slice-1 behavior
  (descriptive ledger only, no enforcement).
- **`Store::session_budget_cap(session_id)`** resolves the
  effective cap (per-session override → default_cap → None).
- **`SessionBudget` envelope** extended with optional `cap` and
  `remaining` fields. JSON shape stays backward-compatible via
  `skip_serializing_if = "Option::is_none"`.
- **`Store::apply_operation_checked` budget gate** (#292 slice 3).
  After typecheck passes, refuses ops that would push the
  session's monotonic spend over the configured cap. New
  `StoreError::BudgetExceeded { session_id, cap, spent_after }`
  variant. Ops without an `intent_id`, with a dangling intent, or
  whose session has no cap configured sail through.
- **HTTP API** maps `BudgetExceeded` to **503 with `Retry-After:
  0`** and a `{kind: "budget_exceeded", session_id, cap,
  spent_after}` detail envelope. Unlike Contention (where retry
  might land later), there's no point retrying as-is — the caller
  needs to raise the cap, switch sessions, or refactor.
- **`lex policy session-budget {set-default <N> | set <id> <N> |
  unbounded <id> | clear <id> | clear-default}`** CLI manages
  the policy file. Writes are atomic (tempfile + rename).
- 14 new tests (9 store-gate + 5 CLI-policy).

With slices 1 + 2 + 3 all shipped, **#292 is complete**.

### Added — agent-safety (#292, slice 1)

- **`Store::session_budget(session_id)`** and
  **`Store::all_session_budgets()`** — read-only ledger of how
  much budget each agent session has spent across every op
  attributed to it via `intent_id → session_id`. New
  `SessionBudget { session_id, spent, op_count }` public type.
- **Spend model**: monotonic. `AddFunction` contributes its full
  `budget_cost`; `ModifyBody` / `ChangeEffectSig` / the typed
  transform variants contribute `max(0, to - from)` (decreases
  don't refund). Ops without `intent_id` or with a dangling
  intent are silently skipped — the ledger degrades gracefully
  rather than failing the read.
- **`lex audit --budget --by-session [--session <id>]`** rolls up
  the ledger and emits per-session spend. Without `--by-session`,
  `--budget` keeps its existing per-sig history shape (#247).
- 8 conformance tests in `crates/lex-store/tests/session_budget.rs`
  (zero on empty / unknown; AddFunction attribution; ops without
  intent skipped; multiple sessions kept separate; ModifyBody
  increase contributes delta only; decrease doesn't refund;
  dangling intent skipped gracefully). 3 CLI tests in
  `crates/lex-cli/tests/audit_session_budget.rs` (empty store;
  filter-by-missing-session zero-pads; `--by-session` requires
  `--budget`).

Slice 1 is **read-only**; slices 2 (`policy.json` `session_budgets`
schema) and 3 (apply-path gate that refuses ops over cap) follow.

### Added — agent-feedback (#281, slice 2a)

- **`lex repair <op_id> --apply --transform '<json>'`** executes a
  typed transform against the branch and emits a `RepairAttempt`
  attestation tied to the original `RepairHint` (#281). Supports
  all four #280 transform kinds (`replace_match_arm`,
  `rename_local`, `inline_let`, `extract_function`) via JSON
  payload. The CLI exits 0 regardless of the apply outcome —
  success/failure lives in the envelope's `outcome` field and in
  the attestation log, not in the exit code.
- **`--branch B`** flag selects which branch the transform
  targets (defaults to the current branch).
- Requires a matching `RepairHint` to exist for the failed op_id;
  bare apply against an unknown op errors with "no RepairHint
  exists."
- The LLM-driven `--apply` mode (no `--transform` flag) ships in
  slice 2b — when supplied without `--transform`, the command
  errors with a "requires `--transform`" message pointing at the
  follow-up slice.
- 3 end-to-end tests in `crates/lex-cli/tests/repair_apply.rs`
  (well-typed transform lands op + records RepairAttempt;
  ill-typed transform records a "failed" RepairAttempt; unknown
  kind surfaces a structured error). Existing flag-validation
  tests in `repair_cli.rs` updated.

## [0.6.0] — 2026-05-10

The agent-onboarding + agent-feedback wave from the post-0.5.0
strategic review. Four issues land in one release:

- **#280 typed refactoring operations** — four AST transforms
  (`ReplaceMatchArm`, `RenameLocal`, `InlineLet`, `ExtractFunction`)
  that let agents express edits as typed deltas rather than as
  raw byte rewrites. The op log finally reads as a semantic edit
  history.
- **#281 closed repair loop, slice 1** — `RepairHint` attestation
  auto-emitted by `apply_operation_checked` on `TypeError`, plus
  `lex repair <op_id>` to read it. The LLM-assisted `--apply`
  path follows in slice 2.
- **#282 `lex docs --for-agent`** — single structured JSON
  envelope giving an agent everything it needs to make sensible
  writes against a Lex repo (workspace, stdlib, recent activity,
  open intents, policy, attention queue).
- **#283 search reindex** — `lex store search reindex` warms the
  embedding cache eagerly. (The HTTP embedder backends and on-
  disk cache turned out to already be shipped under #224 — the
  PR also corrects the stale "deferred to slice 2" note in
  `lex-search`'s lib.rs.)

Minor bump because every data-layer change is additive: new
`OperationKind` variants (`ReplaceMatchArm`, `RenameLocal`,
`InlineLet`) follow the established `skip_if_none` discipline so
pre-0.6.0 OpIds stay stable; new `AttestationKind` variants
(`RepairHint`, `RepairAttempt`) are append-only enum extensions;
new `lex` subcommands (`docs`, `repair`, `op gc`, `search reindex`)
add surface without changing existing CLIs.

### Added — agent-feedback (#281)

- **`AttestationKind::RepairHint { failed_op_id, errors,
  suggested_transform }`** — auto-emitted by
  `Store::apply_operation_checked` when an op is rejected for a
  `TypeError`. Attached to each candidate stage in the rejected
  transition. The hint records the *would-be* op_id (deterministic,
  content-addressed even though the op record is never persisted)
  and the structured errors. `suggested_transform` is left `None`;
  a future slice (`lex repair --apply`) will populate it via LLM.
- **`AttestationKind::RepairAttempt { hint_id, outcome,
  applied_op_id }`** — variant for recording repair iterations.
  Schema-only in this slice; producers land with the LLM path.
- **`lex repair <op_id> [--store DIR]`** — read-only walk of the
  attestation log surfacing the latest matching `RepairHint`.
  Emits `{ found, failed_op_id, stage_id, attestation_id,
  timestamp, errors, suggested_transform }` JSON. The
  LLM-assisted `--apply` mode is deferred to a follow-up slice
  and explicitly errors with "not yet implemented" today.
- 3 conformance tests in `crates/lex-store/tests/repair_hint.rs`
  (TypeError emits hint, success emits no hint, deterministic
  failed_op_id across retries) plus 3 CLI tests in
  `crates/lex-cli/tests/repair_cli.rs` (no-hint reporting,
  --apply rejection, text rendering). Existing
  `apply_operation_checked_emits_no_attestation_on_rejection` is
  renamed to `..._does_not_emit_typecheck_attestation_on_rejection`
  and now asserts the RepairHint is the sole attestation written.

### Added — agent-VCS roadmap (#280, slice 4)

- **Typed `ExtractFunction` transform** (#280 slice 4). Closes the
  last of the four typed refactoring operations from the 0.5.0
  review. New `lex_ast::extract_function(stage, expr_node, spec)`
  produces `(modified_source, new_fn)`: the original stage with
  the extracted sub-expression replaced by a call, and the new
  top-level fn carrying the extracted body.
- **`ExtractFnSpec`** carries the agent-provided signature
  (name, type_params, params, return_type, effects). The
  transform verifies that `spec.params` covers exactly the free
  variables of the extracted expression — names must match; extra
  params or missing free vars are refused with
  `TransformError::ExtractFnRefused`. Type checking against the
  spec happens *after* the transform in the apply path.
- **`Store::apply_extract_function`** emits two ops linked by a
  shared synthetic `Intent`: an `AddFunction` for the new fn and
  a `ModifyBody` for the source's call-site rewrite. The intent's
  prompt is structured (`[lex.transform.extract_function]\nnew_fn=...`)
  so `lex op log --intent <id>` recovers the typed-transform
  shape from the op-log + intent-log join. No new
  `OperationKind` variant — the typed view comes from the intent
  linkage.
- 5 unit tests in `lex_ast::transforms` (replacement, extra-param
  refusal, missing-param refusal, zero-free-var case, TypeDecl
  rejection) and 5 conformance tests in
  `crates/lex-store/tests/extract_function.rs` (two linked ops,
  structured intent, branch-state consistency, transform-error
  surface, param-mismatch surface).

**#280 is now complete** — all four typed refactoring transforms
(`ReplaceMatchArm`, `RenameLocal`, `InlineLet`, `ExtractFunction`)
are shipped with end-to-end conformance.

### Added — agent-VCS roadmap (#280, slice 3)

- **Typed `InlineLet` transform** (#280 slice 3). Eliminates a
  `let x := v; body` by substituting `v` for every unshadowed `x`
  in `body`, then replacing the `Let` node with the substituted
  body. New `lex_ast::inline_let(stage, let_node)` runs the
  transform deterministically.
- **Safety restrictions**: `v` must be capture-free and
  side-effect-free — only `Literal`, `Var`, `FieldAccess`, and
  `BinOp`/`UnaryOp`/`TupleLit`/`ListLit` trees over those leaves
  are accepted. Calls, lambdas, blocks, lets, matches in `v` are
  refused with `TransformError::InlineLetRefused` so a future
  slice can lift the restriction without changing the error
  contract. Free-variable capture (a free var of `v` is re-bound
  in `body`) is detected and refused too.
- **`OperationKind::InlineLet { sig_id, from_stage_id,
  to_stage_id, let_node, binding_name, from_budget, to_budget }`**
  records the inlined name + position. Same `skip_if_none` budget
  discipline as the other transform variants.
- **`Store::apply_inline_let`** wraps the end-to-end flow.
- 5 unit tests (substitution, call refusal, capture refusal,
  shadowing, not-a-Let target). 4 conformance tests
  (`crates/lex-store/tests/inline_let.rs`) covering op landing,
  AST reconstruction, attestation emission, transform-error
  surface.

The fourth and final slice (`ExtractFunction`) remains a
follow-up on #280.

### Added — agent-VCS roadmap (#280, slice 2)

- **Typed `RenameLocal` transform** (#280 slice 2). Second of the
  typed refactoring operations from the 0.5.0 review. New
  `lex_ast::rename_local(stage, let_node, new_name)` walks the
  binding's body scope and rewrites every unshadowed reference to
  the old name. Pure function — same shape as
  `lex_ast::replace_match_arm`.
- **Scope-aware**: shadowing by inner `Let`, `Lambda` params, or
  `Match` pattern bindings cuts off the rewrite. `value`-side
  references are left untouched because Lex `let` is
  non-recursive.
- **`OperationKind::RenameLocal { sig_id, from_stage_id,
  to_stage_id, let_node, old_name, new_name, from_budget,
  to_budget }`** records the rename in the op log; same
  `skip_if_none` budget discipline as the rest of #280's variants.
- **`Store::apply_rename_local`** end-to-end: load AST → transform
  → publish new stage → re-typecheck → emit op. Failure modes
  match `apply_replace_match_arm`: `TransformError` for malformed
  targets, `TypeError` for ill-typed results, `InvalidTransition`
  for no-op renames.
- 6 unit tests in `lex_ast::transforms` (rename + body refs;
  no-op refusal; inner-`Let` shadow; `Lambda` param shadow;
  `Match` pattern shadow; not-a-Let target). 5 conformance tests
  in `crates/lex-store/tests/rename_local.rs` (lands typed op,
  `get_ast` reconstruction, no-op refusal, TypeCheck attestation,
  transform-error surface for wrong node).

Slices for `InlineLet` and `ExtractFunction` remain follow-ups.

### Added — agent-discovery (#283)

- **`lex store search reindex`** warms the embedding cache eagerly
  by walking every active stage through the configured embedder
  (#283). With `LEX_EMBED_URL` set, hits Ollama (`/api/embeddings`)
  or OpenAI-compat (`/v1/embeddings`) per `LEX_EMBED_PROVIDER`;
  without it, falls back to `MockEmbedder`. Reports
  `{ indexed, dim, elapsed_ms, store }` as the JSON envelope.
- **Updated lib-level docs** in `lex-search` to reflect what's
  actually shipped: the slice-2-deferred items (HTTP embedder
  backends, on-disk cache) are in fact present (`HttpEmbedder`
  with both Ollama and OpenAI wire formats, `CachingEmbedder`
  with SHA-256-keyed disk cache). HNSW and cross-store cache sync
  are the genuinely-deferred items.
- 2 conformance tests in `crates/lex-cli/tests/search_reindex.rs`:
  reindex on an empty store reports zero, and only `Active`
  stages count (drafts are skipped — fixes the "every publish
  inflates the index with stale candidates" failure mode).

### Added — agent-onboarding (#282)

- **`lex docs --for-agent`** emits a single structured JSON
  envelope with the six sections an agent needs to make sensible
  writes against a Lex repo: `workspace` (lex version, branches,
  current/default), `stdlib` (every active sig on the branch with
  rendered type signature, effects, optional budget),
  `recent_activity` (last N ops with op_id, kind tag, sig_id,
  intent_id), `open_intents` (intents referenced by recent ops,
  resolved against `IntentLog`), `policy` (required_attestations,
  blocked_producers, gc_retention from `policy.json`), and
  `attention` (stages with active `Block` attestations).
- `--branch B`, `--limit-recent N` (default 50), `--store DIR`
  flags. Schema versioned via `lex_docs_version` so future agents
  can detect breakage.
- Pure derivation from existing on-disk state — no new
  persistence, no LLM calls, no costs beyond the per-section
  walks.
- 7 conformance tests in `crates/lex-cli/tests/docs_for_agent.rs`
  driving the CLI as a subprocess.

### Added — agent-VCS roadmap (#280, slice 1)

- **Typed `ReplaceMatchArm` transform** (#280 slice 1). First of
  the typed refactoring operations identified as the load-bearing
  primitive for "agents write Lex by typed delta, not by raw
  bytes." A new `lex_ast::replace_match_arm(stage, match_node,
  arm_index, new_body)` produces a `Stage` with one arm's body
  replaced; pattern preserved.
- **`OperationKind::ReplaceMatchArm { sig_id, from_stage_id,
  to_stage_id, match_node, arm_index, from_budget, to_budget }`**
  records the transform's intent in the op log. Semantically a
  `ModifyBody`, but the op carries *what* changed and *where* in
  the AST — so the log reads as a semantic edit history rather
  than as opaque hash-to-hash bytes. Same `skip_if_none` budget
  discipline as #247's existing variants — pre-#280 OpIds stay
  stable.
- **`Store::apply_replace_match_arm`** end-to-end: loads source
  AST → runs transform → publishes the new stage → re-typechecks
  the candidate program → emits the typed op. Failure modes:
  `StoreError::TransformError` (transform didn't apply),
  `StoreError::TypeError` (transform succeeded but result is
  ill-typed, branch unchanged), or `InvalidTransition` for no-op
  edits.
- 6 unit tests in `lex_ast::transforms` (splice/apply primitives,
  arm bounds, kind mismatch, error surfaces) and 6 conformance
  tests in `crates/lex-store/tests/replace_match_arm.rs` (lands
  typed op, reconstruction via `get_ast`, TypeCheck attestation
  emission, ill-typed rejection with unchanged branch, transform-
  error surfacing, no-op rejection).

Slices for `RenameLocal`, `InlineLet`, `ExtractFunction` (the
other three transforms in #280) will land separately.

## [0.5.0] — 2026-05-09

The op-log performance roadmap and the post-0.4.0 limitation
follow-ups. #261 ships in three slices (packfiles, predicate-driven
GC, delta-encoded stages) so the on-disk layout scales past the
~10k-op cap that 0.4.0's loose-file model implied. Alongside, the
limitation issues filed against 0.4.0 are now closed (#256
walk-back gate, #257 ops-during-run trace attestations, #258
attestation-cascade migration), the multi-writer concurrency story
is real (#262 CAS branch advance), and `lex op pull` (#260)
completes the symmetric inverse of #242's push.

Minor bump because every data-layer change is additive — packfiles
and `.delta.json` are new file shapes alongside the existing loose
forms; loose-file readers ignore them. The one breaking surface is
the dep update: `rand 0.10` and `getrandom 0.4` rotated their
trait imports (`OsRng` → `SysRng`, `TryRngCore` → `TryRng`,
`getrandom::getrandom` → `getrandom::fill`), so callers of
`lex_vcs::Keypair::generate` or the `crypto.random` runtime
handler that took out their own RNG will need to adjust imports.

### Added — agent-VCS roadmap (#261, slice 3)

- **Delta-encoded stage bytes** (#261 slice 3). When
  `Store::publish` lands a stage and the byte diff against the
  most-recent prior stage in the same SigId's lifecycle is below
  `DELTA_RATIO_THRESHOLD` (50%), the stage is persisted as
  `<stage_id>.delta.json` instead of the full
  `<stage_id>.ast.json`. The format is a content-stable splice:
  `(base_stage_id, common_prefix_len, common_suffix_len,
  middle_hex)` — applying it produces `base[..prefix] + middle +
  base[tail..]`.
- **`Store::get_ast`** transparently walks the delta chain to
  reconstruct canonical bytes, then parses. Callers see no
  difference between full-snapshot and delta-encoded stages.
- **Chain-length cap** (`DELTA_CHAIN_CAP = 32`). Every delta
  records its chain length; when the next publish would exceed
  the cap, the publish path falls back to a full snapshot. Keeps
  worst-case `get_ast` cost bounded.
- **Determinism**: the splice picks the largest common prefix,
  then the largest non-overlapping suffix, so a given
  `(base_bytes, new_bytes)` pair always produces the same
  `.delta.json`.
- 6 unit tests in `delta.rs` (splice/apply round-trips, pure
  insertion, pure deletion, threshold/cap gating, overflow guard)
  and 7 conformance tests in `delta_conformance.rs` (first stage
  is a full snapshot, close stages delta-encode, transparent
  reconstruction, lifecycle round-trip, idempotent republish,
  deep-chain snapshot materialization, dissimilar fallback).

This closes the slice-3 acceptance criteria from #261. With
slices 1, 2, and 3 all merged, the op-log performance roadmap
on #261 is fully shipped.

### Added — agent-VCS roadmap (#261, slice 2)

- **Predicate-driven op-log GC** (#261 slice 2). New
  `Store::plan_gc(cli_retain) -> GcPlan` and
  `Store::apply_gc(plan)`. Three retention rules combine to form
  the surviving set: (1) every op reachable from any branch head
  is always kept, (2) ops matching any retain predicate (CLI args
  + `policy.gc_retention.retain` entries) are kept, (3) every
  parent of a retained op is kept transitively (DAG integrity —
  honors the "refuse to delete an op that's still a parent of a
  retained op" criterion). Each retained op carries a
  `RetentionReason` for the plan envelope.
- **`policy.json` `gc_retention.retain`** schema — opaque
  `serde_json::Value` predicates so the policy file is forward-
  compatible with future `Predicate` variants. Empty (default)
  means "no extra retention; branch-reachable ops only."
- **`OpLog::evict(victims)`** removes op_ids across both loose
  files and packfiles. Pack handling: any pack containing a
  victim is rewritten to a new content-addressed pack with only
  the survivors (or deleted outright when no survivors remain).
- **`lex op gc {--dry-run|--confirm} [--retain JSON ...] [--store DIR]`**
  CLI command. `--dry-run` reports the plan without touching
  disk; `--confirm` applies. Idempotent — re-running on a
  GC'd store finds no new orphans.
- 7 conformance tests in `crates/lex-store/tests/gc_conformance.rs`
  covering reachability, predicate match, parent closure, pack
  rewriting, idempotence, and policy.json wiring.

### Added — agent-VCS roadmap (#261, slice 1)

- **Op-log packfiles** (#261 slice 1). Loose-file storage at
  `<store>/ops/<op_id>.json` is fine to ~10k ops; past that the
  filesystem starts to thrash. `OpLog::repack(threshold)` now
  consolidates loose records into a deterministic, content-
  addressed packfile pair: `pack-<hash>.pack` (each record framed
  as `[8-byte BE length][canonical JSON]`, ops sorted by op_id)
  and `pack-<hash>.idx` (JSON map of op_id → byte offset).
- **Pack name** is the SHA-256 of the sorted op_ids joined by
  newlines, so re-running `lex op repack` on the same input
  always produces the same pack hash — a no-op rather than a
  rewrite.
- **`OpLog::get`** falls back from loose → pack on miss; every
  walk method (`walk_back`, `walk_forward`, `lca`, `ops_since`)
  works seamlessly across both. `list_all` dedups via op_id, so
  an interrupted repack (loose + pack both present) still yields
  exactly one record per op.
- **`lex op repack [--threshold N] [--store DIR]`** (default
  threshold 1000). No-op below threshold; emits a JSON envelope
  reporting `packed: N`.
- Crash safety: `.pack.tmp` and `.idx.tmp` are fsync'd before
  rename; loose files are deleted only after both renames
  succeed. A crash mid-repack leaves both forms on disk; `get`
  finds the loose copy and a subsequent repack cleans up.
- Conformance: 1000 loose ops → repack → every `OpLog::get`
  returns the byte-identical record, plus determinism and
  no-op-on-already-packed tests.

Slices 2 (predicate-driven GC) and 3 (delta encoding) remain
follow-up work tracked under #261.

### Added — agent-VCS roadmap (#257)

- **Ops-during-run trace attestations** (#257). Closes the
  follow-up #246 documented: `Trace` attestations gained an
  `op_id` field but no producer set it. Now `Store::record_op_trace`
  emits per-stage Trace attestations with `op_id: Some(...)`
  linking a committed op to the run that produced it, and
  `Store::record_run_committed_ops_since` walks the
  `ops_since(branch_head, base)` diff and emits Trace
  attestations for each new op.
- **`lex run --trace`** snapshots the default branch's `head_op`
  before the VM exits and calls `record_run_committed_ops_since`
  after, so any ops committed during the run get linked to the
  run automatically. Common case (no ops committed) is a single
  no-op call returning 0.
- **`lex trace --op <op_id>`** is now populated by this pipeline
  — the filter the surface added in #246 finally returns hits.
- **`StoreError::UnknownOp`** for `record_op_trace` against an
  op_id that isn't in the log.

### Added — agent-VCS roadmap (#262)

- **Multi-writer CAS branch advance** (#262). Pre-#262, two writers
  calling `Store::apply_operation` concurrently on the same branch
  could lose one another's update — `Branch.head_op` was a
  read-modify-write under no lock. Now branch advance is a
  fs2-advisory-locked CAS on `<branches>/<name>.lock`: the
  retry loop reads the current head, persists the op (idempotent
  via content addressing), then CAS-advances against the
  pre-persist parent. CAS mismatch rebuilds the op with the new
  head and retries up to 32 times before surfacing
  `StoreError::Contention { branch, attempts }`.
- **`Store::set_branch_head_op_cas`** + **`CasFailed`** internal
  primitives. Single-parent ops are rebuildable (the retry loop
  swaps the parent and re-derives the op_id); merge ops are not
  (their parents are meaningful), so a CAS mismatch on a merge
  surfaces immediately as `Contention`.
- **HTTP API**: `apply_operation`-routing endpoints (`/v1/patch`,
  `/v1/branches/.../merge/.../commit`, `/v1/programs/publish`)
  map `Contention` → 503 with a `Retry-After: 1` header and a
  `{kind: "contention", branch, attempts}` detail envelope.
- Conformance tests in `crates/lex-store/tests/concurrent_apply.rs`:
  N-thread races land every writer's signature, the resulting
  history is a single linear chain, and op records are never lost
  on retry (orphaned records are intentional under the append-only
  contract).

### Added — agent-VCS roadmap (#258)

- **Attestation-cascade migration** (#258). Closes the documented
  limitation in #244. When an `OperationFormat` rotation
  invalidates every `OpId`, `lex_vcs::migrate::plan_attestation_migration`
  + `apply_attestation_migration` re-derive every attestation
  whose `op_id` references a rotated op. New attestations get a
  fresh `attestation_id` (content-addressed, including the new
  op_id) and inherit the `by-stage` and `by-run` indices.
- **`AttestationLog::delete`** — narrowly-purposed cleanup of
  primary file + by-stage + by-run index entries, used only by
  the cascade.
- **`lex store migrate-ops --confirm`** now invokes the
  cascade after the op-log migration: rotates op_ids, rewrites
  branch heads, cascade-migrates attestations, and invalidates
  every branch's `last_gate_checkpoint` (#256). Reports
  `attestations_rotated: N` in the JSON envelope.
- **Idempotency**: re-running the cascade on an already-migrated
  log is a no-op; the original mapping's keys (old op_ids) no
  longer exist, so plan returns an empty step list.

### Added — agent-VCS roadmap (#256)

- **Walk-back producer-block gate** (#256). Closes the
  grandfathering window in #248. Previously, a retro-block of a
  compromised tool only fenced off *new* ops; pre-existing
  contamination in branch history was grandfathered in. Now the
  gate walks back from `head_op` to the per-branch
  `last_gate_checkpoint`, runs `check_producer_block` on every
  ancestor's attestable stages, and refuses if any contamination
  is found.
- **`Branch.last_gate_checkpoint: Option<OpId>`** persisted on
  disk. Every successful advance moves the checkpoint to the new
  head; subsequent advances are `O(new ops)` because the walk
  stops at the checkpoint. Pre-#256 branch files default to
  `None` (serde default) and trigger a one-time full walk on
  next advance — same backward-compat trick `intent_id` (#131)
  used.
- **`Store::invalidate_gate_checkpoints()`** clears every
  branch's checkpoint. `lex attest retro-block` and
  `lex attest retro-unblock` call it after writing the
  attestation, so the next advance re-walks and surfaces (or
  clears) any newly contaminated ancestors. Reported as
  `branches_invalidated: N` in the CLI's JSON envelope.

### Added — agent-VCS roadmap (#260)

The symmetric inverse of #242. With both push and pull, content-
addressed sync is bidirectional: any agent on any machine can
both publish and consume ops + attestations from a remote.

- **`GET /v1/ops/since?after=<op_id>&branch=<name>&limit=<n>`** —
  server endpoint accepting a since-cutoff and returning a JSON
  array of `OperationRecord`s reachable from `branch.head_op` but
  not from `<after>`, sorted **oldest-first** so the client can
  apply with the existing idempotent `OpLog::put`. Empty array
  when caller is at the remote's head, when the branch doesn't
  exist, or when the remote is behind. `--limit` chunks large
  gaps.
- **`GET /v1/attestations/since?after-op=<op_id>&limit=<n>`** —
  mirror for the attestation log. Excludes attestations whose
  `op_id` is in the cutoff's ancestry; always ships
  attestations with `op_id: None` (e.g. `Override`,
  `ProducerBlock`).
- **`lex op pull <remote_url> [--branch NAME] [--since OP_ID]
  [--limit N] [--dry-run]`** CLI command. Probes the remote head,
  fetches the delta, validates each record (content-addressing +
  DAG integrity), persists via `OpLog::put`. On clean
  fast-forward (local head is an ancestor of the new tip) the
  branch advances; on divergent histories the pull refuses with
  a `DivergentHistory { local_head, remote_head }` envelope and
  the local branch is unchanged.
- **`lex attest pull <remote_url> [--since-op OP_ID] [--limit N]
  [--dry-run]`** mirrors the same shape for attestations.
  Attestations whose `op_id` references an op the local doesn't
  yet have are skipped (with a `rejected_unknown_op` count) so
  the caller can re-issue after pulling the missing ops.

### Out of scope (called out for follow-up)

- **Bidirectional `lex sync`** that wraps `pull && push` — a CLI
  veneer, not a new protocol primitive.
- **`--force-fast-forward`** that overwrites local divergent
  history. Route through the merge engine (#134) instead.
- **Capability-scoped pull** (only fetch ops matching effect set
  X). Symmetric to the same gap on the push side.

## [0.4.0] — 2026-05-08

The agent-VCS roadmap. Closes the seven gaps from the lex-vcs
review: a positive attestation-gate on branch advance, retroactive
producer quarantine, an op-log ↔ trace link, cost accounting on
ops, append-only sync over HTTP, plus the foundational OpId
stability spec and operation format versioning that already
shipped in 0.3.0's tail. Minor bump because every change is
additive at the data layer (Option fields with
`skip_serializing_if`, new enum variants, new endpoints) — no
existing OpId rotates and no on-disk record changes shape.

### Added — agent-VCS roadmap (#248)

- **Retroactive producer quarantine** (#248). Two new
  `AttestationKind` variants — `ProducerBlock { tool_id, reason,
  blocked_at }` and `ProducerUnblock { tool_id, reason,
  unblocked_at }` — let an admin declare "as of T, attestations
  produced by tool X are no longer trusted." The branch advance
  gate consults the latest verdict per `tool_id` (by timestamp;
  ties go to `ProducerUnblock`) and refuses to advance over an op
  whose stage carries an attestation produced by a quarantined
  tool at or after the cutoff. Distinct from `policy.json`'s
  `blocked_producers` (#181), which is a forward-going read-time
  tag for the activity feed; this is a write-time gate.
- **`StoreError::ProducerBlocked`** with a structured
  `ProducerBlocked` envelope (`error: "ProducerBlocked"`,
  `op_id`, `stage_id`, `tool_id`, `blocked_at`, `attestation_at`,
  `attestation_id`). Distinct error code from #245's
  `BranchAdvanceBlocked` so the HTTP API can route the security
  failure separately from the missing-evidence failure.
- **`lex attest retro-block --producer <tool_id> --reason "..."`**
  and **`lex attest retro-unblock --producer <tool_id> --reason
  "..."`** CLI commands. Emit attestations under
  `stage_id == tool_id` so the existing by-stage index doubles as
  a by-tool lookup — no schema break, no separate index needed.
- **Ops are not deleted.** The attestation log carries the
  tombstone; the op log stays append-only. Branch advance is
  refused, but the audit trail is intact.

#### Limitation (called out for follow-up)

- **Walk-back gate is not implemented.** Today's gate only checks
  the *new* op being advanced past, not its ancestors. Pre-
  existing contamination in a branch's history is grandfathered
  in at the moment of the retro-block. A future "walk back to the
  last successful gate run" pass would close this gap.

### Added — agent-VCS roadmap (#245)

- **Machine-checkable branch advancement gates** (#245). New
  `policy.required_attestations` rules in `<store>/policy.json`:
  every op landing on the branch must carry a `Passed` attestation
  of the listed kinds before `Store::apply_operation` will advance
  the branch head past it. Conditions: `always` (every op) or
  `effects_intersect: [io, fs_write, …]` (only when the op's
  declared effects intersect). Surfaces a structured
  `BranchAdvanceBlocked { op_id, stage_id, missing }` error;
  `to_envelope()` renders it as the JSON shape the HTTP API will
  serve.
- **TypeCheck attestation now emits before the gate, not after.**
  `apply_operation_checked` is reordered:
  typecheck → persist op → emit `TypeCheck` → run gate → advance
  head. A policy that requires `TypeCheck` is satisfied by the
  auto-emission for every freshly-checked op, with no caller
  changes.
- **`lex policy require-attestation <kind> [--when-effects e1,…]`**
  and **`lex policy unrequire-attestation <kind>`** CLI commands.
  Supported kinds: `type_check`, `spec`, `sandbox_run`, `examples`,
  `diff_body`, `effect_audit`. The pre-#245 `lex policy list`
  command is renamed `lex policy show` (alias `list` retained) and
  now renders both `blocked_producers` and `required_attestations`
  in one view.

### Added — agent-VCS roadmap (#246)

- **`AttestationKind::Trace { run_id, root_target }`** (#246).
  Closes the deferred decision in `docs/design/trace-vs-vcs.md`:
  option B (a dedicated variant) is now in production, replacing
  the option-A workaround of overloading `SandboxRun` with an
  empty effect set. `lex attest filter --kind trace` returns
  *only* trace attestations.
- **`AttestationLog::list_for_run(run_id)`** + new
  `<root>/attestations/by-run/<run_id>/` secondary index. Only
  `Trace` variants are indexed; other kinds skip it. Cost is
  `O(traces of that run)`, typically 1.
- **`lex run --trace`** auto-emits a `Trace` attestation linking
  the trace blob to the entry function's `stage_id` (#246).
  `result` mirrors the run's success: `Passed` / `Failed`. Skipped
  silently when the entry function isn't resolvable to a stage in
  the loaded program.
- **`lex trace --op <op_id>`** lists every `Trace` attestation
  whose `op_id` field matches. Today empty unless an
  ops-committed-during-a-run pipeline populates `op_id`; the
  surface is in place for that follow-up.
- **`lex attest filter --run <id>`** uses the new by-run index.
- `docs/design/trace-vs-vcs.md` updated: option B documented,
  rationale captured, follow-ups list pruned.

### Added — agent-VCS roadmap (#247)

- **Cost accounting on ops** (#247). Three `OperationKind`
  variants gain optional budget fields:
  - `AddFunction { …, budget_cost: Option<u64> }`
  - `ModifyBody { …, from_budget: Option<u64>, to_budget:
    Option<u64> }`
  - `ChangeEffectSig { …, from_budget: Option<u64>, to_budget:
    Option<u64> }`

  All use `#[serde(default, skip_serializing_if =
  "Option::is_none")]` — same trick that `intent_id` (#131) used —
  so pre-#247 ops without a declared budget keep byte-identical
  canonical bytes and their existing `OpId`s. The golden test
  from #243 still passes against the unchanged hash.
- **`budget_from_effects(effect_set)`** parser. Pulls the literal
  `n` out of `"budget(n)"` labels in an `EffectSet`. `compute_diff`
  uses it to populate the new fields end-to-end so the op log
  records the budget the type-checker saw, without rehydrating
  stages at query time.
- **`lex op show`** renders a `cost: 50 → 100 (+100%)` line for
  ops that carry a budget delta. `from → to (signed-pct%)`,
  `→ N` for an Add, `N → (unset)` when the budget disappears,
  `N → N (no change)` for ModifyBody where only the body moved.
- **`lex op log --budget-drift [PCT]`** filters the log to ops
  whose declared `[budget]` cost grew or shrank by at least
  `PCT` percent (default 10%). Each kept row carries a
  `budget_drift_pct` field in the JSON output.
- **`lex audit --budget`** walks the op DAG on the current
  branch and reports per-`SigId` budget history: initial cost,
  current cost, and the chain of `(op_id, from, to)` changes.
  JSON envelope under `--output json` for agent consumers.

### Added — agent-VCS roadmap (#242)

The biggest gap from the agent-VCS review: until now, the op log
was local-only. Two agents on different machines couldn't
exchange ops without filesystem-level sharing. #242 closes that
with **append-only sync**: content-addressed identity makes
replication a set-difference of immutable blobs, not a merge.

- **`POST /v1/ops/batch`** — server endpoint accepting a JSON
  array of `OperationRecord`s. Validates DAG integrity (every
  parent must already exist on the remote *or* appear earlier
  in the same batch — so a topologically-ordered slice from
  `OpLog::ops_since` lands in one round-trip), and the
  content-addressing invariant (`op_id` must equal the canonical
  hash of the supplied payload).
- **`POST /v1/attestations/batch`** — mirrors the ops endpoint
  for attestations. Rejects records whose `op_id` field
  references an op the remote doesn't know about; rejects
  `attestation_id` mismatches.
- **`GET /v1/branches/<name>/head`** probe endpoint so the
  client can compute a delta against the remote's current head
  before pushing.
- **`lex op push <remote_url> [--branch NAME] [--since OP_ID]
  [--dry-run]`** CLI command. Walks `OpLog::ops_since(local_head,
  remote_head)`, batches, posts in topological order. Reports
  `received / added / skipped` from the server's response.
  `--dry-run` previews without network calls.
- **`lex attest push <remote_url> [--since-op OP_ID]
  [--dry-run]`** mirrors the same shape for the attestation log.

Failure modes (all return structured envelopes):

- `422 MissingParent { op_id, missing_parent }` — DAG integrity.
  Whole batch rejected; nothing persisted.
- `422 UnknownOp { attestation_id, op_id }` — attestation
  references an op the remote doesn't have.
- `409 OpIdMismatch { supplied, expected }` /
  `409 AttestationIdMismatch { supplied, expected }` —
  content-addressing was forged or corrupted in transit.
- `400` for malformed JSON.

Idempotency is built in at every layer: `OpLog::put` and
`AttestationLog::put` are no-ops on existing ids. Pushing the
same payload twice returns `added: 0` on the second call.
Network failure mid-push leaves the remote with the prefix that
landed; re-running picks up cleanly.

### Out of scope (called out for follow-up)

- **Pull / fetch.** Append-only log replication is unidirectional
  in this slice. The inverse (pulling someone else's ops) is
  tracked as a separate piece of work.
- **Capability-scoped sync** (only push ops matching effect set
  X) — natural follow-up once a use case appears.
- **Auth.** Plumbed through the existing `lex serve` surface;
  not redesigned here.
- **Conflict resolution.** Ops are immutable and content-
  addressed; there are no conflicts to resolve at the transport
  layer. The merge engine already handles agreement on shared
  history.

### Added — agent-VCS roadmap (#244)

- **`OperationFormat` enum + version-aware canonical encoder**
  (#244). `Operation::canonical_bytes_in(format)` and
  `Operation::op_id_in(format)` route through a per-format encoder.
  Today only `OperationFormat::V1` is in production; the
  infrastructure exists so a future `OperationKind` schema change
  is an explicit, migrate-able event rather than a silent
  invalidation of every existing `OpId`.
- **`OperationRecord.format_version`** persisted in the on-disk
  JSON. Pre-#244 records (no field) deserialize to `V1` (serde
  default); V1 records continue to omit the field on write
  (`skip_serializing_if = is_implicit`), so adding it doesn't
  rotate any existing `OpId` or change any on-disk byte.
- **`lex_vcs::migrate`** module with `plan_migration` /
  `apply_migration`. Two-phase rewrite: write all new
  `<new_op_id>.json` files, then delete old ones. Crash mid-
  migration leaves both old and new files coexisting, each
  internally consistent. Topological order ensures parents are
  remapped before any child references them.
- **`lex store migrate-ops --to <format> [--dry-run | --confirm]`**
  CLI command. Required `--dry-run` or `--confirm` because the
  `--confirm` path is destructive (deletes old op-log files,
  rewrites every `<root>/branches/*.json` head_op through the
  mapping). Reports the old→new op_id mapping for every op.

### Internal

- **V1→V2 migration rewrites every `OpId`.** When a future
  `OperationFormat::V2` lands, the canonical pre-image bytes will
  change for every op, so every `OpId` rotates. The migration tool
  is the only safe path; manual surgery on `<root>/ops/` will
  break parent-chain integrity. The `--dry-run` mode is mandatory
  reading before committing.
- **Attestation cascade is a known follow-up.** `AttestationId` is
  computed including `op_id`, so rotating op_ids leaves
  attestations dangling (their stored `op_id` field points to
  deleted records, and their own ids are now stale). The
  `migrate-ops` command warns about this; an attestation-log
  migration is a separate piece of work tracked alongside #244.

## [0.3.0] — 2026-05-08

Stage signing, semantic search over the store, a stable binary
canonical-AST format, refinement types end-to-end, a much larger
stdlib, and an optimizer-pass track for agent runtimes. Minor
bump because per-capability effect parameterization changes
`EffectSet.concrete` and content-addressed closure identity
changes `Value::Closure` — both API-visible.

### Added — type system & spec-checker

- **Refinement types end-to-end** (#209 slices 1+2+3). `{x: Int |
  x > 0}` is a first-class type the spec-checker verifies at
  gate time. Predicates compose, and a function whose return
  type is refined carries the predicate into call sites.
- **Spec-checker ADTs** (#208 slice 3). User-defined sum types
  are consumable in spec bodies. The `Allow / Deny /
  Inconclusive` verdict surface is unchanged.
- **Bounded list quantifiers** (#208 slice 2). `forall x in xs,
  P(x)` and `exists x in xs, P(x)` are evaluated eagerly by the
  gate.
- **Per-capability effect parameterization** (#207). Effect rows
  carry argument lists, so `[net("wttr.in")]` and
  `[net("api.internal")]` are statically distinguishable. Bare
  `[name]` absorbs any `[name(...)]` (subsumption);
  `[name(arg)]` matches only itself. `--allow-effects` accepts
  bare (`mcp`), CLI-colon (`mcp:ocpp`), or canonical
  (`mcp(ocpp)`) forms. `lex-vcs::EffectSet` preserves args
  end-to-end (#223).

### Added — canonical AST + signed stages

- **Stable binary canonical-AST format** (#206 slice 1). New
  encoder/decoder under `lex-ast`. Round-trip-stable, version-
  prefixed, identity-preserving against the existing content-
  addressed `AstId`.
- **`lex canonical encode` / `lex canonical decode`** (#206
  slices 2+3) and **`lex run --from-canonical FILE`** for
  executing a canonical-AST file directly. Closes the loop
  where an agent fetches an AST from one store and runs it
  locally without ever materialising source.
- **ed25519-signed stages** (#227). `lex publish --sign-with
  KEYFILE` writes a detached signature into stage metadata.
  **`lex run --from-store STAGE_ID`** with `--require-signed` /
  `--trusted-key HEX` verifies before the AST is loaded;
  tampered metadata fails fast even without `--require-signed`.
  `--trusted-key` implies `--require-signed`.

### Added — semantic search over the store (#224)

New `lex-search` crate. Agents find stages by intent rather
than by exact name.

- **`lex store search "<query>"`** and **`lex audit --query
  "<text>"`** rank active stages by fused cosine over
  description / signature / examples (weights 0.5 / 0.3 / 0.2,
  redistributed when a field is absent). Examples scoring uses
  max-pool so one strong example anchors the stage.
- **`MockEmbedder`** (slice 1) — deterministic SHA-256 bag-of-
  words, L2-normalised, 64 dims. Keeps the test suite offline
  and byte-stable.
- **`HttpEmbedder`** (slice 2) — Ollama (`POST
  /api/embeddings`, per-text) or OpenAI-compat (`POST
  /v1/embeddings`, batched). `LEX_EMBED_URL`,
  `LEX_EMBED_PROVIDER`, `LEX_EMBED_MODEL`, `LEX_EMBED_API_KEY`.
- **`CachingEmbedder<E>`** — on-disk cache under
  `<store>/search/embeddings/`, sharded by SHA-256 prefix and
  fingerprinted by `provider:model` so swapping providers never
  returns vectors of the wrong shape. Atomic temp+rename
  writes; corrupt files fall back to a fresh upstream call.

### Added — stdlib

- **`std.cli`** (#240). Argparse-equivalent for end-user
  programs: flags, options (`--name value` and `--name=value`),
  positionals, subcommands with their own flag namespace, `--`
  end-of-options, ACLI-shaped envelope, plus `cli.help` /
  `cli.describe` introspection.
- **`std.parser`** (#217). Structural parser combinators with
  **`parser.map` / `parser.and_then`** (#221).
- **`std.random`** (#219). Pure, seeded RNG. Same seed, same
  sequence; no global state.
- **`std.env`** (#216). Runtime env-var access through the
  effect system.
- **`std.math` extensions.** Trig, transcendentals, rounding,
  and 2-argument forms (`atan2`, `pow`, …).
- **`result.or_else` / `option.or_else`.** Recovery combinators
  symmetric with `and_then`. `option.and_then`'s signature is
  now registered with the type-checker.

### Added — runtime correctness & optimizer (#231)

- **Runtime budget enforcement** (#225). The `[budget(N)]`
  effect is now enforced at every `Op::Call` / `Op::TailCall` /
  `Op::CallClosure` via a new
  `EffectHandler::note_call_budget(cost)` trait method.
  `DefaultHandler` deducts atomically via CAS against an
  `Arc<AtomicU64>` pool; a deduction that would underflow
  returns `"budget exceeded: requested N, used so far M,
  ceiling C"` *before* mutating the pool. Conservative
  accounting — failed calls still consume their declared cost.
  No ceiling = no enforcement, preserved.
- **Dead-branch elimination on canonical AST** (#228). `match
  LITERAL { … }` (and the desugared form of `if true { … }`)
  folds to the live arm before type-checking, so effects that
  lived only in dead code drop out of the inferred effect set.
- **Memoization** and **retry+backoff** for retryable effects.
- **Content-addressed closure identity** (#222). Two closures
  with the same captured environment and body produce the same
  `ClosureId` — automatic dedup across the store.
- **Agents-only track** (#230). Stdlib batch + closure
  canonicality + per-capability effects threaded through the
  runtime path agent runtimes use.

### Added — runtime ergonomics (#240)

- **`str.slice` clamping.** Out-of-range bounds clamp to `[0,
  s.len()]` (Python semantics for the common case);
  mid-codepoint and `lo > hi` after clamping still error so
  UTF-8 truncation can't sneak through silently.
- **`parse_strict` nested required fields.** The required-
  fields list accepts dotted paths (`"project.license"`, three-
  level descent works) and `\.` for literal-dot field names.
  Top-level case unchanged.

### Added — parser & docs

- **`_name` identifiers and `_` discard in `let`** (#200,
  #205). `let _ = side_effect()` and `let _name = …` for
  intentionally-unused bindings.
- **Cross-compile recipes** (#198, #204) for aarch64 Linux and
  Apple Silicon, plus a **post-publish release smoke test**
  (#232) that runs the cross-compiled binaries in CI.

### Changed

- **`EffectSet.concrete` shape change** (#207, #223). Effect
  rows now carry `EffectKind { name, arg: Option<EffectArg> }`
  instead of bare `String`. `EffectSet::singleton(s)` keeps its
  old signature (constructs a bare `EffectKind`); the new
  `EffectSet::singleton_arg(name, arg)` constructs the
  parameterized form. Downstream code reading raw effect rows
  will see the new shape.
- **`Value::Closure` shape change** (#222, #230). Closures now
  carry a content-addressed identity. Code matching on
  `Value::Closure` directly will need to update.
- **Diagnostics name parameterized effects.** Policy violations
  render `mcp("ocpp")` rather than just the bare kind.

### Internal

- Workspace bumped to 0.3.0; 39 inter-crate `version = "0.2.2"`
  specifiers across `crates/*/Cargo.toml` updated together with
  the workspace version.

## [0.2.2] — 2026-05-06

Real wires for the `[llm_local]` and `[llm_cloud]` effects, plus
a connection cache for `agent.call_mcp`. Stub responses are gone.

### Added

- **`agent.local_complete(prompt)`** (#196) hits Ollama (or any
  HTTP-compatible service) at `OLLAMA_HOST` (default
  `http://localhost:11434`), model from `LEX_LLM_LOCAL_MODEL`
  (default `llama3`), and returns the completion text.
- **`agent.cloud_complete(prompt)`** (#196) hits any
  OpenAI-shape chat-completions endpoint. Provider-agnostic by
  design — point `LEX_LLM_CLOUD_BASE_URL` at OpenAI / Mistral /
  Groq / Together / DeepSeek / vLLM / etc. API key from
  `LEX_LLM_CLOUD_API_KEY` (preferred) or `OPENAI_API_KEY`
  (fallback). Model from `LEX_LLM_CLOUD_MODEL`.
- **EffectHandler escape hatch** documented (`crates/lex-runtime/src/llm.rs`).
  Custom auth, batching, alternative providers, or non-HTTP
  transports go through wrapping `DefaultHandler` and
  intercepting the dispatch — no upstream change needed.
- **`McpClientCache`** (#197): LRU-bounded cache of stdio MCP
  clients keyed by command-line string, default cap 16. Per-
  `DefaultHandler` instance. Subprocess death is detected
  lazily — failed `tools/call` drops the client so the next
  call respawns. Replaces the spawn-per-call pattern from
  v0.2.0.

### Changed

`agent.local_complete` / `agent.cloud_complete` no longer
return the `Ok("<llm_local stub>")` / `Ok("<llm_cloud stub>")`
sentinels. Existing callers must either:

- Set `OLLAMA_HOST` / `OPENAI_API_KEY` (etc.) so the call
  succeeds, or
- Wrap `DefaultHandler` and intercept the dispatch with a
  custom `EffectHandler` impl.

`agent.send_a2a` keeps its stub — that wire format lives in
the downstream `soft-a2a` crate, not in lex-lang.

### Internal

- Workspace bumped to 0.2.2 (additive surface; the stub
  behaviour change is API-visible but explicitly documented as
  a v1 → v2 transition).

## [0.2.1] — 2026-05-06

Patch release, additive only.

### Added

- **`spec_checker::evaluate_gate_compiled_traced`** (#199). Opt-in
  tracer hook on the runtime gate: callers pass a
  `Fn() -> Box<dyn Tracer>` factory and every Vm the gate spins
  up for `SpecExpr::Call` is wired to a fresh tracer. Lets a
  downstream agent runtime (e.g. `soft-agent`) capture the spec
  body's nested host-helper calls (`under_budget` →
  `projected_load + budget_total`) into the same trace tree as
  the rest of the action.
- **`lex_trace::Handle: Tracer + Clone`** (supports #199).
  Multiple `Vm` instances can take their own
  `Box::new(handle.clone())` and the events fold into the same
  shared `Recorder` state. Existing `impl Tracer for Recorder`
  unchanged.

### Changed

Nothing breaking. Existing callers of `evaluate_gate*` and
`Recorder` keep their signatures and semantics.

## [0.2.0] — 2026-05-06

First public release on crates.io. The 10 library crates listed
in [`crates published`](#crates-published-in-this-release) ship
at this version; the rest carry `publish = false`.

### Added — agent-runtime primitives (#184–#192)

Driven by the `soft` proposal's request for a typed substrate
to build agent runtimes on. The four `std.agent` builtins below
each carry their own effect tag so a function declared
`[llm_local, a2a]` cannot accidentally reach `[llm_cloud]` or
`[mcp]`; the type-checker enforces this at compile time.

- **`std.agent` module** with effect-typed builtins (#184):
  - `agent.local_complete(prompt) :: [llm_local] Result[Str, Str]`
  - `agent.cloud_complete(prompt) :: [llm_cloud] Result[Str, Str]`
  - `agent.send_a2a(peer, payload) :: [a2a] Result[Str, Str]`
  - `agent.call_mcp(server, tool, args_json) :: [mcp] Result[Str, Str]`
- **Real stdio MCP client** behind `agent.call_mcp` (#185).
  JSON-RPC 2.0 over a subprocess; spawn-per-call. Connection
  cache is a v2 follow-up pending downstream benchmarks.
- **Spec-checker as a runtime gate** (#186). New
  `evaluate_gate(specs, bindings, lex_source) -> GateVerdict`
  API: per-action `Allow / Deny / Inconclusive` verdicts in
  single-digit milliseconds for small spec sets. The randomized
  property checker stays as the offline tool.
- **Type-driven `parse[T]` validation** for `std.{json,toml,yaml}`
  (#168, #188). When the inferred result type is
  `Result[Record{...}, _]` the type-checker rewrites the call to
  validate required fields before returning `Ok`.
- **`docs/design/trace-vs-vcs.md`** (#187) — traces stay out of
  the op log; cross-store sync uses attestations for metadata
  plus content-addressed blob copy for the trace JSON. No new
  resolver needed.

### Crates published in this release

- `lex-syntax` — tokenizer + parser
- `lex-ast` — canonical AST + content-addressed identity
- `lex-types` — type system + effect inference
- `lex-bytecode` — bytecode compiler + VM
- `lex-runtime` — effect handler runtime + capability policy
- `lex-trace` — trace tree + replay
- `lex-vcs` — agent-native VCS (typed op log + attestation graph)
- `lex-store` — on-disk store (stages, branches, traces)
- `lex-api` — HTTP/JSON + MCP server surface
- `spec-checker` — property checker + runtime gate

Internal crates (`core-syntax`, `core-compiler`, `lex-stdlib`,
`lex-cli`, `conformance`) carry `publish = false`. Install the
`lex` binary via `cargo install --git
https://github.com/alpibrusl/lex-lang lex-cli` until a binary
release flow is in place.

### Added — agent-native VCS, lex-tea v3 (#172, #181)

- **`Override` / `Defer` / `Block` / `Unblock` attestation kinds**
  (#177, #178). Human triage actions are first-class
  attestations, queryable via `lex attest filter --kind ...`.
- **`lex stage pin / defer / block / unblock`** CLI commands;
  `lex stage pin` consults `lex_vcs::is_stage_blocked` and
  refuses to activate a blocked stage.
- **Web UI parity** for triage actions on `/web/stage/<id>`
  (#179).
- **`<store>/users.json`** actor-identity gate (#180).
  `LEX_TEA_USER` env var and `X-Lex-User` header both validated
  against the file when present; v3a–v3c behaviour preserved
  when absent.
- **`lex merge defer <merge_id> <conflict_id>`** per-conflict
  shortcut (#182). `Resolution::Defer` plumbed through.
- **`<store>/policy.json`** producer block list (#183). The
  activity feed renders a `blocked` tag next to attestation
  rows whose `produced_by.tool` is on the list. Read-time
  enforcement; the attestation log keeps every record.
- **`lex policy block-producer / unblock-producer / list`** CLI
  commands.

### Added — earlier

- **MCP server** (`lex serve --mcp`) exposing the v1 JSON API as
  MCP tools (#175, #171).
- **Closures-as-values in record fields** (#176, #169).
- **Agent-native VCS, tier-2 — full rollout.** Closes #128 (and
  sub-issues #129-#134). The store goes from a snapshot-of-functions
  database to a **typed event log with first-class intent and
  durable evidence**. Implementation arrived as ~25 small PRs
  through #135-#162; entries below are the user-visible surfaces.
  - **Operation log as the store's source of truth (#129).** New
    `crates/lex-vcs/`. Typed `Operation` (the unit-of-write
    replacing snapshot-of-tree) with content-addressed `OpId`
    (SHA-256 over canonical-form `(kind, payload, sorted
    parents)`). Two agents producing the same logical change
    against the same parent state get the same `OpId` — automatic
    dedup. Op kinds: `AddFunction`, `RemoveFunction`, `ModifyBody`,
    `RenameSymbol`, `ChangeEffectSig`, `AddImport` / `RemoveImport`,
    `AddType` / `RemoveType` / `ModifyType`, `Merge`. `lex publish`
    emits typed ops; the per-branch `head_op` advances atomically.
    `lex op show` / `lex op log` and `lex blame` causal-history
    walk the DAG.
  - **Write-time type-check gate (#130).** `Store::publish_program`
    and `Store::apply_operation_checked` reject any op whose
    candidate program doesn't typecheck. The HEAD invariant —
    "every accepted op produces a typechecking program" — is
    structural rather than convention. `POST /v1/publish` returns
    422 on `StoreError::TypeError` with the structured envelope.
  - **First-class `Intent` (#131).** Persistent record linking an
    op to its originating prompt, agent session, and model. Ops
    can be queried by intent via predicate branches; `lex blame`
    surfaces "who/why" alongside "what/when".
  - **Attestation graph — durable, queryable evidence (#132).**
    Six attestation kinds:
    - `TypeCheck` — auto-emitted by the store-write gate on every
      accepted op.
    - `Spec` — emitted by `lex spec check --store DIR` and `lex
      agent-tool --spec --store DIR`. Records the verdict
      (`Passed` / `Failed { detail: counterexample }` /
      `Inconclusive`) plus method (`Random` / `Exhaustive` /
      `Symbolic`) and trial count.
    - `Examples` / `DiffBody` / `SandboxRun` — emitted by `lex
      agent-tool` on each verification step (`--examples`,
      `--diff-body`, the final sandboxed run).
    - `EffectAudit` — emitted by `lex audit --effect K --store
      DIR`. Per stage: `Passed` if it doesn't touch any of the
      listed effects, `Failed { detail: "touches forbidden
      effect(s): ..." }` otherwise.

    Failures persist alongside successes — a flaky producer can't
    overwrite negative evidence by re-running. Consumers:
    - `GET /v1/stage/<id>/attestations` — the JSON list.
    - `lex stage <id> --attestations` — CLI mirror.
    - `lex blame --with-evidence` — per-stage history with
      attestations attached to each entry.
    - `lex attest filter --kind K --result R --since T` —
      cross-stage queries for CI / dashboards. `--since` accepts
      epoch seconds or `YYYY-MM-DD`.
  - **Predicate-defined branches (#133).** Branches become saved
    queries over the op log, not snapshots. `Branch.predicate :
    Option[Predicate]`; the engine in `lex_vcs::predicate` handles
    `All` / `Intent` / `Session` / `AncestorOf` / `And` / `Or` /
    `Not`. Cheap to create + discard (`O(1)` — a small JSON file
    per branch). New CLI surfaces:
    - `lex branch peek <other> [--since-fork] [--vs <branch>]` —
      read another branch's ops without switching, optionally
      restricted to ops since the fork point. Eliminates "context
      blindness" as a query rather than a merge.
    - `lex branch overlay <other> [--on <branch>]` — preview a
      merge result without committing: the dst head map projected
      forward over auto-resolved sigs, plus the conflict list.
    - Existing `lex branch create <name> [--from BRANCH |
      --predicate '<json>']` learned the `--predicate` form for
      saved-query branches.
  - **Programmatic merge API (#134).** Stateful merge sessions
    that agent harnesses can drive iteratively without text
    editing or merge markers.
    - `POST /v1/merge/start` returns conflicts as structured
      objects with typed `ours` / `theirs` / `base` stage_ids.
    - `POST /v1/merge/<id>/resolve` accepts batched resolutions
      (`TakeOurs` / `TakeTheirs` / `Custom { op }` / `Defer`)
      with per-conflict verdicts. `Custom` extracts the merge
      target from the op kind via `OperationKind::merge_target`.
    - `POST /v1/merge/<id>/commit` lands a `Merge` op with
      parents `[dst_head, src_head]`; auto-resolved Src outcomes
      and TakeTheirs resolutions become entries in the
      `StageTransition::Merge` map.
    - CLI mirror: `lex merge {start | status | resolve | commit}`
      persists sessions to `<store>/merges/<merge_id>.json` so
      each invocation is its own process.
  - **`lex-api` POST /v1/publish returns 422 on StoreError::TypeError
    (#146).** Same shape as `/v1/check`'s 422 — clients have one
    error contract for both surfaces.
  - **End-to-end example: `examples/agent_merge/`.** A scripted
    walkthrough: two "agents" diverge on `clamp`, produce a
    `ModifyModify` conflict, a third resolves it programmatically,
    the final body's spec attestation lands. Maps each step to
    the relevant tier-2 issue.
  - **Performance budgets** (smoke tests under `tests/branch_perf.rs`
    and `tests/merge_perf.rs`): 100 branch create+delete cycles
    under a 1k-op store < 1s; 50-conflict resolve+commit cycle
    < 250ms. Catches quadratic regressions; full-scale (10k-op)
    benchmarking is left as a `cargo bench` follow-up.
- **`lex-tea` v1 — read-only HTML browser over lex-vcs (#163).**
  Three pages on the same `lex serve` process — no extra binary,
  no extra port, no SPA build:
  - `GET /` — branch list with current-branch marker.
  - `GET /web/branch/<name>` — fns on a branch with stage_id
    links.
  - `GET /web/stage/<id>` — stage info plus the full attestation
    trail (auto-emitted TypeChecks, plus any persisted Spec /
    Examples / SandboxRun / EffectAudit).

  CSS is one short embedded blob; zero JS. The point is to expose
  what makes lex-vcs different — typed ops, attestations,
  evidence trails — to humans without a frontend pipeline. JSON
  API at `/v1/*` is unchanged. `lex-tea` will grow into a
  Gitea-equivalent (merge UI, comments via Intent, basic auth)
  in subsequent slices.
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

[0.2.2]: https://github.com/alpibrusl/lex-lang/releases/tag/v0.2.2
[0.2.1]: https://github.com/alpibrusl/lex-lang/releases/tag/v0.2.1
[0.2.0]: https://github.com/alpibrusl/lex-lang/releases/tag/v0.2.0
[0.1.0]: https://github.com/alpibrusl/lex-lang/releases/tag/v0.1.0
