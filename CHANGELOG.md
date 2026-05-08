# Changelog

All notable changes to lex-lang. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
versioning follows [SemVer](https://semver.org/) (pre-1.0; minor
bumps may carry breaking changes when justified).

## [Unreleased]

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
