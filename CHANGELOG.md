# Changelog

All notable changes to lex-lang. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
versioning follows [SemVer](https://semver.org/) (pre-1.0; minor
bumps may carry breaking changes when justified).

## [Unreleased]

### Changed

- **#388: HTTP server runs on `hyper` + `tokio` instead of `tiny_http`
  (plaintext path).** `net.serve_fn(port, closure)` and
  `net.serve(port, handler_name)` now drive their listener through a
  multi-threaded Tokio runtime; `hyper::server::conn::http1::Builder`
  handles request parsing, response serialisation, and HTTP/1.1
  keep-alive. The synchronous Lex VM call still happens per request,
  but on Tokio's `spawn_blocking` pool so the accept loop isn't
  starved while a handler runs.

  Lex-facing surface is unchanged — handlers still receive a Lex
  `Request` record (`method`, `path`, `query`, `body`, `headers`),
  return a `Response` (or the structural `{ body, status }` shape
  pre-#375), and `[net]` is still the only effect required.
  Streaming bodies (`BodyStream` / `BodyBytes`, from #375) now flow
  through `http_body_util::StreamBody` and emit one HTTP chunk per
  Lex iter item under `Transfer-Encoding: chunked`. The pre-#375
  bare-`Str` escape hatch for handlers that ignore the registered
  `Response` alias keeps working.

  **Performance.** The bench harness in
  [`lex-web#2`](https://github.com/alpibrusl/lex-web/pull/2) shows
  ~7.7k RPS / 28ms p50 for hello-world on tiny_http. With hyper the
  same hardware should land ~40-60k RPS / ~3-5ms p50 (depending on
  HTTP/1.1 pipelining + keep-alive). Concrete numbers depend on the
  TFB-style bench rerun that goes alongside this PR; tests in
  `crates/lex-runtime/tests/net_serve.rs` include a new
  `net_serve_keeps_connection_alive` case that exercises two
  sequential requests on one TCP connection — impossible under the
  one-thread-per-conn tiny_http path.

  **What's deferred.** `net.serve_tls(port, cert, key, handler)`
  still uses `tiny_http`'s `ssl-rustls` backend. Migrating it to
  `tokio-rustls` + `hyper` is a follow-up; we don't drop the
  `tiny_http` dep yet. The interpreter optimisations in #389
  (computed-goto dispatch, inline caches, per-request arena) are
  the next bottleneck once the HTTP server stops being the floor.

### Fixed

- **#399: `Policy::permissive()` was missing several stdlib effect
  kinds**, causing `lex test` (which uses the permissive policy by
  default) to reject test files that touched stdlib modules whose
  effects weren't in the runner's allow-list. Most painful case:
  `[sql]` from `std.sql`, which broke any test that reached the SQL
  surface — blocking lex-orm and lex-ocpp test suites under
  `lex ci`. Added `sql`, `random` (`crypto.random` /
  `crypto.random_str_hex`), `chat`, `log`, `kv`, `stream`, and
  `fs_walk` to the permissive set; commented the rule that
  `Policy::permissive()` tracks the stdlib effect catalog so future
  effect additions get the same treatment.

### Added

- **#399: `lex test --allow-effects k1,k2,...` flag.** Lets test
  runs override the runner's permissive policy with an explicit
  allow-list — covers vendor-extension effects we don't ship in the
  stdlib catalog, and (in the other direction) lets contributors
  verify effect-shape contracts by restricting a test run to a
  tight allow-list. Mirrors `lex run --allow-effects` exactly.

- **#390: `net.dial_ws` — WebSocket client primitive.** Inverse of
  `net.serve_ws_fn` (server, shipped in 0.9.0): open an outbound
  WebSocket connection, fire `on_open` once after the handshake,
  then loop invoking `on_message` for every inbound frame.

  ```lex
  fn net.dial_ws[E](
    url         :: Str,                    # ws:// or wss://
    subprotocol :: Str,                    # e.g. "ocpp1.6", or "" for none
    on_open     :: () -> [E] WsAction,
    on_message  :: (WsMessage) -> [E] WsAction,
  ) -> [net, E] Result[Unit, Str]
  ```

  Both callbacks return the same `WsAction` enum (`WsSend(Str)` /
  `WsSendBinary(List[Int])` / `WsNoOp`) as the server-side
  `serve_ws_fn` — the action is applied to the socket immediately
  after the closure returns. `on_message` is also invoked once with
  `WsClose` before the loop exits, so handlers can run cleanup. The
  return type is `Result[Unit, Str]` (not bare `Unit` like the
  server side) because a dial can fail on connect (DNS, refused,
  bad TLS), bad URL, or mid-stream (read/write error), and the
  caller usually wants to act on that.

  TLS / `wss://` URLs are supported via tungstenite's
  `rustls-tls-webpki-roots` feature — production OCPP / signed-WS
  endpoints work out of the box, no extra config.

  **v1 limitation.** The issue proposed a `send` closure threaded
  through both handlers so users could push outbound frames from
  arbitrary `[net]` code (e.g. a charger's heartbeat scheduler).
  That requires representing Rust-native closures as Lex `Value`s,
  which is a separate runtime change tracked for a follow-up
  release. v1 covers the BootNotification + reactive-reply pattern
  that motivates the issue: `on_open() => WsSend(boot_frame)` plus
  `on_message(WsText(s)) => WsSend(handle(s))` is enough to write a
  conforming OCPP charge-point client.

  Closes #390.

## [0.9.2] — 2026-05-13

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

- **#376: lazy `Iter[T]` via `iter.unfold(seed, step)`.** The
  positional `Iter[T]` stdlib added in 0.9.1 (#364) gains a true lazy
  constructor: `iter.unfold(seed :: S, step :: (S) -> Option[(T, S)])
  -> Iter[T]`. The step closure is invoked once per `iter.next` call;
  returning `None` ends the iteration. Internal `Iter[T]` representation
  changed from `(List[T], Int)` to a tagged Variant
  (`__IterEager(list, idx)` / `__IterLazy(seed, step)`); the variant
  names are `__`-prefixed so user code can't construct or match them
  and `Iter[T]` stays opaque from the type system's perspective. Both
  `iter.next` and `iter.to_list` dispatch on the variant — eager iters
  use the existing positional cursor, lazy iters call the step closure
  and re-wrap the result. Unblocks unbounded sources (range generators,
  paged-API fetchers) without materialising into a list first. The
  other iter ops (`map`, `filter`, `take`, `fold`, etc.) remain eager-
  only in this release; call `iter.to_list` first if you need them on
  a lazy iter. Closes #376.

- **#379: streaming SQL cursor — `sql.query_iter[T]`.** New
  `sql.query_iter[T](db, q, params) -> [sql] Result[Iter[T], Str]`
  returns rows one at a time through an `Iter[T]` instead of
  materialising the full result set into a `List[T]`. Backed by a
  per-cursor mpsc channel (capacity 64 rows, LRU-bounded at 256
  cursors per process); the producer thread blocks at backlog so
  resident memory stays bounded regardless of result-set size.
  SQLite uses `Statement::query`'s row iterator; Postgres opens a
  transaction with `DECLARE … CURSOR FOR …` and loops on `FETCH 64`.
  Dropping the `Iter[T]` closes the cursor and releases the connection.
  Pairs with #376 (lazy iter) for true row-by-row streaming downstream.
  Closes #379.

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

- **`lex init`: AI-assistant-ready scaffold (PR #396).** `lex init`
  now produces a project that's `lex ci`-green from minute one and
  ready for an AI assistant to start working in:
  - **`AGENTS.md`** dropped at the project root — short cold-start
    guide for Claude Code / Cursor / Aider / Copilot covering
    install (cross-platform `curl + tar -xz` from GitHub Releases),
    the `lex check → lex test → lex fmt → lex ci` loop, the 6-8
    Lex-isms most likely to trip up a model (`::` vs `:=`,
    effects-as-types, `Result`/`Option`, `examples { }` blocks),
    and pointers to upstream `docs/AGENT.md`.
  - **CI workflow pins to the scaffolding toolchain version** —
    `LEX_VERSION: v<version>` env at the top of
    `.github/workflows/lex.yml`. The install step downloads the
    pre-built binary tarball from GitHub Releases instead of
    `cargo build`-ing (~30s vs ~3min, no Rust required). Same
    `LEX_VERSION` recipe in `AGENTS.md` keeps local and CI on the
    same toolchain.
  - **`lex ci` step appended** to the workflow alongside the four
    explicit named steps. The named steps stay so failures remain
    categorised in the GH Actions UI; the trailing `lex ci` is a
    belt-and-braces full repro of the local command.
  - **`src/main.lex` and `tests/test_main.lex` carry `examples { }`
    blocks** — surfaces the convention from line 1 of every new
    repo. Bug fix: the previous test stub had `fn run_all() -> ()
    { ... }` which failed type-checking (`expected ()`, `got Unit`),
    leaving a fresh `lex init` red on minute one — now returns
    `Int` 0.
  - **`docs/QUICKSTART.md`** added to lex-lang itself — single URL
    an operator can hand to an AI assistant (`Implement <project>
    in Lex. Follow <URL>`) covering empty-repo to green CI without
    duplicating per-project AGENTS.md content.

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

### Fixed

- **#391: `examples { }` block name resolution across cross-file
  imports.** When the multi-file loader pulled in an imported file,
  it rewrote references in the function body to the file's mangled
  prefix but passed the `examples { }` block through unchanged — so
  a fn whose example called another top-level fn in the same file
  type-checked cleanly inside the defining package but failed with
  `unknown_identifier` when imported from another package. Loader
  now mangles example `args` and `expected` with the same rules as
  the body (with an empty shadow set, since examples sit outside
  the param scope). Self-references continue to work because the
  fn's own mangled name is registered in `local_names`.

- **#395: `lex test` aliased imports unresolved
  (`unknown_identifier` on every `import … as X`).** `lex test`'s
  per-file runner called `parse_source` directly on each test file,
  skipping the multi-file loader entirely. Aliased imports like
  `import "../src/lib" as lib` or `import "pkg-name/mod" as p` never
  had their aliases bound or their referenced files merged into the
  program; the same file that passed `lex check` failed `lex test`.
  Routes test files through `lex_syntax::load_program` so they get
  the same import expansion, mangling, and stdlib pass-through as
  `cmd_check`. Effectively unblocked `lex test` (and therefore
  `lex ci`'s test step) for every multi-module project.

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
  tests in `