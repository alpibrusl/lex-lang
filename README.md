# lex-lang

[![CI](https://github.com/alpibrusl/lex-lang/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/alpibrusl/lex-lang/actions/workflows/ci.yml)
[![fuzz](https://github.com/alpibrusl/lex-lang/actions/workflows/fuzz.yml/badge.svg?branch=main)](https://github.com/alpibrusl/lex-lang/actions/workflows/fuzz.yml)
[![License: EUPL-1.2](https://img.shields.io/badge/license-EUPL--1.2-blue.svg)](LICENSE)
[![Rust 1.80+](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](#building-from-source)

**The contract layer agents emit into.** Lex is a typed-effect language built for the case where an LLM, not a human, is the primary author. Every function declares its effects; the type checker rejects any body that lies about what it touches, *before a byte runs*. The content-addressed AST and append-only operation log survive the next ten model upgrades.

## See it

**Demo 1 — Effects are the contract** (25 s): honest effect rows are accepted; a body that lies about touching the network is rejected at type-check time.

[![effects are the contract](https://asciinema.org/a/LD0axLDi3Izw2Ibw.svg)](https://asciinema.org/a/LD0axLDi3Izw2Ibw)

**Demo 2 — Two agents, structural merge** (90 s): two agents modify the same function on separate branches; the merge conflict is a typed JSON record, not a diff marker; every step — publish, branch, merge, spec — lands as a content-addressed attestation.

[![two agents, structural merge](https://asciinema.org/a/nHXhh17X92ykGX2O.svg)](https://asciinema.org/a/nHXhh17X92ykGX2O)

```sh
bash examples/manifesto_effects/demo.sh   # demo 1
bash examples/agent_merge/demo.sh         # demo 2
```

## How it works

**1. Sandbox — effects-as-types, pre-execution rejection.**
Every function declares its effects (`[io]`, `[net]`, `[fs_write("/tmp/…")]`, `[llm_cloud]`, …). The checker refuses any body that reaches outside the declaration. Lex blocks 7/7 adversarial cases pre-execution; RestrictedPython blocks 3/7. Full report: [`bench/REPORT.md`](bench/REPORT.md).

**2. Repair — structured hints, typed transforms.**
When a type-check fails, the gate emits a `RepairHint` with a `suggested_transform` derived from a static `(rule_tag → transform)` table. The LLM fix path runs as a typed op, not free-text rewriting. Every attempt lands as a `RepairAttempt` attestation.

**3. Typed-transform VCS — content-addressed stages, structural diff.**
The store is an append-only log of typed operations (`AddFunction`, `ModifyBody`, `ReplaceMatchArm`, `Merge`, `Candidate`, `Promote`, …). Conflicts surface as JSON records, not `<<<<<<` markers. `lex blame --with-evidence` walks the full attestation chain.

**4. Coordination — session budgets, ProducerTrust, multi-agent.**
Multiple proposers race via `Candidate / Promote` without CAS contention. Per-session budget gates cost across all participating agents. `ProducerTrust` scores tools against a rolling window of attestations.

**5. JIT tier-up — Cranelift native compilation.**
Hot functions are promoted from the bytecode interpreter to native code via Cranelift. The current backend is a phase-1 MVP covering an op subset (pure-int arithmetic; no closures/records yet). On that subset a steady-state micro-benchmark shows 84–194× over the interpreter — a *lower bound* for JIT ROI; real programs land between that and 1× ([`crates/lex-jit/benches`](crates/lex-jit/benches), [`docs/design/jit-roadmap.md`](docs/design/jit-roadmap.md)).

**6. Package registry — publish and consume via LexHub.**
`lex pkg publish` packs a `lex.toml` project and ships it to a registry. Consumers declare `{ registry = "…", version = "…" }` dependencies; the resolver downloads and caches the archive on first use. The canonical registry is [LexHub](https://lexhub.alpibru.com).

## Quickstart

```sh
# Build.
cargo build --release
export PATH="$(pwd)/target/release:$PATH"

# Type-check: pure function passes; effectful function shows a grant hint.
lex check examples/a_factorial.lex    # → ok
lex check examples/c_echo.lex         # → ok  required effects: io

# Run with an explicit effect grant.
lex run examples/a_factorial.lex factorial 5         # → 120
lex run --allow-effects io examples/c_echo.lex echo '"hello"'  # → hello

# Sandbox an LLM-emitted body — rejected before it runs if it lies.
lex agent-tool --allow-effects net --input "url" \
  --body 'match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }'
# → TYPE-CHECK REJECTED  exit 2

# Start the HTTP/JSON agent API on :4040 (add --mcp for a stdio MCP server).
lex serve &
curl -sX POST localhost:4040/v1/check \
  -H 'content-type: application/json' \
  -d '{"source":"fn add(x :: Int, y :: Int) -> Int { x + y }"}'
# → {"ok": true}

# Publish a package to LexHub.
cd my-lex-project
lex pkg publish --token $LEX_PUBLISH_TOKEN   # registry = "..." in lex.toml
```

## Examples

| Example | What it shows |
|---|---|
| [`manifesto_effects/`](examples/manifesto_effects/) | Effects are machine-verifiable constraints — honest rows pass; lying rows are rejected |
| [`agent_merge/`](examples/agent_merge/) | Structural VCS: two agents diverge, merge is a typed op with JSON conflicts |
| [`agent_sprint/`](examples/agent_sprint/) | Four-stage pipeline — each stage narrows to exactly the effects it needs |
| [`weather_app.lex`](examples/weather_app.lex) | Single-handler REST API with `[net]`-scoped effects |
| [`analytics_app.lex`](examples/analytics_app.lex) | CSV → group-by → JSON; `--allow-fs-read` scope |
| [`inbox_app.lex`](examples/inbox_app.lex) | Webhook router — adding a network call to the spam handler is a type error |
| [`agent_dispatcher.lex`](examples/agent_dispatcher.lex) | `[proc]` effect with binary allow-list; typed argv |

## Packages

Packages published to [LexHub](https://lexhub.alpibru.com) — the canonical Lex package registry. Add them to `lex.toml` and run `lex pkg install`.

| Package | Version | What it is |
|---|---|---|
| [**lex-schema**](https://github.com/alpibrusl/lex-schema) | 0.9.2 | Pydantic-style runtime validation, codegen, and schema utilities. `required_str`, `optional_*`, `ModelSchema`, JSON validation. |

```toml
# lex.toml
[dependencies]
lex-schema = { registry = "https://lexhub.alpibru.com", version = "0.9.2" }
```

**Publishing your own package:**

```sh
# lex.toml must have [package] name, version, and registry fields.
lex pkg publish --token $LEX_PUBLISH_TOKEN
```

Tokens are issued by the LexHub operator. See [`lex-hub`](https://github.com/alpibrusl/lex-hub) for self-hosting.

## Supply-chain provenance

A published package can carry a **signed capability contract** — the same format the [lex-os](https://github.com/alpibrusl/lex-os) capsule runtime installs — binding the published bytes to the **grant the code actually needs**, so a consumer verifies *what it's getting* before trusting it, and a runtime runs it at *least authority*.

```sh
# Publish: derive the required grant from the code's typed effects, then sign.
lex pkg publish --sign $KEY --derive-grant \
    --contract-out weather.contract.json --archive-out weather.tar
#   the contract's grant is the union of the entrypoint's declared effects —
#   the publisher can't over- or under-declare; it's provably least authority.

# Verify a package against its contract (signature · content hash · signer).
lex pkg verify --archive weather.tar --contract weather.contract.json \
    --trusted-keys keyring.json

# Install: verify every registry dependency's contract before trusting it.
lex pkg install --trusted-keys keyring.json --require-contracts
#   refuses a substituted archive (integrity), a forged contract (authenticity),
#   or a signer you didn't pin (authorization) — the gates capsule install uses.
```

Trust is **earned, not just pinned**. An install lands as a durable, content-addressed attestation, and a publisher's track record becomes the keyring the next install consults:

```sh
lex attest import-install --audit install.audit.json   # install → attestation graph
lex producer-trust recompute --tool $SIGNER            # score the publisher's record
lex producer-trust keyring --min-trust 700 --out keyring.json   # earned trusted-keys
```

**`bash demo/supply-chain.sh`** runs the whole loop end-to-end against a stand-in registry (one `lex` binary, no network): derive-grant publish → verify-on-install → a tampered-archive refusal → durable attestation → an earned keyring. The same contract and keyring then drive `lex-os capsule install`.

## Ecosystem

Tooling and runtime libraries that extend the Lex platform:

| Package | What it is |
|---|---|
| [**lex-hub**](https://github.com/alpibrusl/lex-hub) | Multi-tenant SaaS gateway — JWT auth, per-tenant stores, package registry host |
| [**lex-agent**](https://github.com/alpibrusl/lex-agent) | Google Agent2Agent (A2A) protocol — AgentCard, JSON-RPC 2.0, SSE streaming |
| [**lex-llm**](https://github.com/alpibrusl/lex-llm) | LLM-agent runtime — Anthropic / OpenAI / Google / Ollama, multi-step tool-call loop |
| [**lex-spec**](https://github.com/alpibrusl/lex-spec) | Capability-precondition DSL — randomized property check + SMT-LIB export |
| [**lex-trail**](https://github.com/alpibrusl/lex-trail) | Content-addressed event log — tamper-evident attestation chains and task replay |
| [**lex-web**](https://github.com/alpibrusl/lex-web) | HTTP router — request-id correlation, gzip, structured access logs |
| [**lex-queue**](https://github.com/alpibrusl/lex-queue) | Redis-backed work queue + pub/sub fan-out |
| [**lex-code**](https://github.com/alpibrusl/lex-code) | Lex-native coding assistant — build / plan / spec / test / review agents |

## Install

**Pre-built binaries** for Linux (x86_64 / aarch64), macOS (x86_64 / aarch64), and Windows are on [GitHub Releases](https://github.com/alpibrusl/lex-lang/releases):

```sh
tar -xzf lex-v0.9.8-x86_64-unknown-linux-gnu.tar.gz
mv lex /usr/local/bin/
lex version
```

**Container image** — multi-arch (`linux/amd64` + `linux/arm64`):

```sh
docker run -p 4040:4040 -v lex-store:/data ghcr.io/alpibrusl/lex:v0.9.8
docker run --rm -v "$(pwd):/work" -w /work ghcr.io/alpibrusl/lex:v0.9.8 check src/main.lex
```

**From source** — requires Rust 1.80+:

```sh
cargo build --release
cargo test --workspace
```

## Status

The core language, sandbox, VCS, and registry are stable and exercised in CI; the Cranelift JIT is an MVP (see below). Key highlights:

- Effect-typed sandbox — 7/7 adversarial blocks pre-execution
- Content-addressed AST + typed Operation log (VCS tier-2)
- Typed transforms: `ReplaceMatchArm`, `RenameLocal`, `InlineLet`, `ExtractFunction`
- Closed repair loop: `lex repair --apply` + `RepairAttempt` attestation
- JIT tier-up — Cranelift native compilation (phase-1 MVP, op subset), 84–194× on hot arithmetic paths
- Trust lattice — effect-narrowing as subtyping + per-host net egress allowlist
- Package registry — `lex pkg publish` + `GET /v1/pkg/{name}/{version}/archive`
- `std.conc` actors, `std.sql` (SQLite + Postgres), `std.crypto`, `std.redis`, `std.http`
- Multi-agent `Candidate / Promote` + per-session budget gate
- `lex-lsp` language server (LSP — VS Code, Cursor, Zed, JetBrains, …), `lex-tea` web UI, MCP server (`lex serve --mcp`), ACLI compliance
- Spec checker (randomized + SMT-LIB export), fuzz CI, conformance harness

Deferred: `flow.parallel_record` (needs row polymorphism), VCS tier-3 federation, JIT slice 5, in-process Z3, store-native imports. Full table: [`docs/STATUS.md`](docs/STATUS.md).

## Docs

| | |
|---|---|
| [`docs/AGENT_GUIDELINES.md`](docs/AGENT_GUIDELINES.md) | Idiom rules — narrow effects, repair loop, `examples {}` blocks, stdlib-first |
| [`docs/AGENT.md`](docs/AGENT.md) | HTTP/JSON API reference (`/v1/check`, `/v1/run`, `/v1/merge/*`, …) |
| [`docs/QUICKSTART.md`](docs/QUICKSTART.md) | Five-minute tutorial |
| [`bench/REPORT.md`](bench/REPORT.md) | Adversarial sandbox comparison — Lex vs Python vs WASM vs gVisor |
| [`CHANGELOG.md`](CHANGELOG.md) | Version history |

---

Built under the principles of [Trust Without Comprehension](https://alpibru.com/manifesto).
