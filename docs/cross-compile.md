# Cross-compiling the `lex` binary

Two paths get you a `lex` binary running on a target you don't
build natively for: pre-built release artifacts (the easy
default) or `cross` (when you need an exact commit / a feature
flag that hasn't been released yet).

## Pre-built release binaries

Every `v*` tag triggers `release.yml`, which produces archives
for these targets and attaches them to the GitHub Release:

| Target | Archive |
|---|---|
| `x86_64-unknown-linux-gnu` | `lex-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| `aarch64-unknown-linux-gnu` | `lex-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| `x86_64-apple-darwin` | `lex-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| `aarch64-apple-darwin` | `lex-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| `x86_64-pc-windows-msvc` | `lex-vX.Y.Z-x86_64-pc-windows-msvc.zip` |

Each archive ships with a `.sha256` sidecar; verify before
running:

```bash
TAG=v0.2.2
TARGET=aarch64-unknown-linux-gnu

curl -sSLO "https://github.com/alpibrusl/lex-lang/releases/download/${TAG}/lex-${TAG}-${TARGET}.tar.gz"
curl -sSLO "https://github.com/alpibrusl/lex-lang/releases/download/${TAG}/lex-${TAG}-${TARGET}.tar.gz.sha256"

sha256sum -c "lex-${TAG}-${TARGET}.tar.gz.sha256"
tar -xzf "lex-${TAG}-${TARGET}.tar.gz"
"./lex-${TAG}-${TARGET}/lex" --version
```

This is the recommended path for **Jetson Orin** (`aarch64-linux`)
and **Mac Studio mini** (`aarch64-darwin`) deployments — the
binaries are signed for those toolchains and don't need any
local Rust install.

## Building from source with `cross`

If you need a build from a specific commit, with a non-default
feature flag, or for a target the release workflow doesn't
cover (e.g. `aarch64-unknown-linux-musl`), use
[`cross`](https://github.com/cross-rs/cross). It runs the Rust
toolchain inside per-target Docker containers, so the host
doesn't need a working sysroot for the target.

### Prerequisites

- Linux or macOS host (cross's Docker images are Linux-based).
- Docker ≥ 20.10 with the `docker` daemon running.
- Rust toolchain (`rustup` is fine).

```bash
cargo install cross --locked
rustup target add aarch64-unknown-linux-gnu
```

### Targeting a Linux ARM64 board (Jetson Orin)

```bash
cross build --release --target aarch64-unknown-linux-gnu --bin lex
# binary at target/aarch64-unknown-linux-gnu/release/lex
```

Copy it to the device:

```bash
scp target/aarch64-unknown-linux-gnu/release/lex jetson:~/
ssh jetson 'chmod +x ~/lex && ~/lex --version'
```

### Targeting macOS ARM64 (Mac Studio mini)

`cross` doesn't support `x86_64-apple-darwin` or
`aarch64-apple-darwin` targets directly — Apple's toolchain
isn't redistributable in a Docker image. Two options:

1. **Build natively on a Mac.** `rustup target add aarch64-apple-darwin` and `cargo build --release --target aarch64-apple-darwin --bin lex`. Same source tree.
2. **Use the pre-built release artifact** (recommended unless you need an unreleased commit).

### A target the release flow doesn't cover

Add it to `cross`'s target matrix and your local rustup:

```bash
rustup target add aarch64-unknown-linux-musl
cross build --release --target aarch64-unknown-linux-musl --bin lex
```

If the build fails because of a C dep that needs a different
sysroot, drop a `Cross.toml` at the workspace root with the
right pre-build hook — see cross's docs.

## Stripping the binary

Both `cargo` and `cross` produce a debug-symbol-laden binary
by default. For production deploys, strip it:

```bash
strip target/aarch64-unknown-linux-gnu/release/lex
# or via cargo:
RUSTFLAGS="-C strip=symbols" cross build --release --target aarch64-unknown-linux-gnu --bin lex
```

Saves ~50% of the binary size; doesn't affect runtime.

## Testing the cross-built binary

A quick smoke test that exercises the in-process bits without
needing a network:

```bash
cat > /tmp/hello.lex <<'LEX'
fn add(x :: Int, y :: Int) -> Int { x + y }
LEX

./lex check /tmp/hello.lex          # should print `ok`
./lex run /tmp/hello.lex add 2 3    # should print `5`
```

For the agent-runtime surface (`agent.local_complete`,
`agent.call_mcp`, the spec gate), see `docs/deploy.md` — those
need configured backends (Ollama / OpenAI-compat / an MCP
server) and aren't useful as a cross-compile smoke test.

## Where this matters

- **`soft` Phase 1** runs vehicle-agents on Jetson Orin and a
  Mac Studio mini per their proposal §6. Smoke-test the
  release artifacts on real hardware before pinning a build.
- **Edge agents in general**. Anything calling
  `agent.local_complete` (which targets a local Ollama by
  default) needs the lex binary on the same machine that's
  running the model server. The pre-built artifacts are the
  fastest route.

## Local-LLM backend choice (downstream concern)

`lex-lang` ships the `lex` binary and the runtime that intercepts
`agent.local_complete` / `agent.cloud_complete` calls. **Choosing
which model server to run alongside it is downstream's job** —
those decisions live with whoever's deploying the agent (`soft`,
in the Phase 1 case).

Common pairings on aarch64 hardware:

| Target | Recommended local-LLM backend |
|---|---|
| Jetson Orin (`aarch64-linux`) | Ollama, or `llama.cpp` with CUDA when latency matters more than ergonomics. |
| Mac Studio mini (`aarch64-darwin`) | Ollama (uses Metal automatically). |

Whatever you pick, point `OLLAMA_HOST` (or the equivalent) at the
right URL before launching `lex run` / `soft-run`. The lex-lang
runtime treats the LLM endpoint as configurable, not pinned —
see `crates/lex-runtime/src/llm.rs` for the env-var precedence
(`LEX_LLM_LOCAL_HOST` → `OLLAMA_HOST` → default
`http://localhost:11434`). Anthropic-shape `agent.cloud_complete`
is similar; see `soft-runner`'s `--llm-cloud-provider anthropic`
path for an example of pointing it at a different endpoint
without a lex-lang change.

The upstream surface that matters here is `#196` — it tracks the
LLM config shape (env vars, header layout, retry / timeout) — and
**not** the choice of which server runs at the other end. That
distinction has been the working assumption between the two
projects since v0.2.0; this doc captures it.

## CI verification

Every tag publish triggers `verify-release.yml`, which downloads
the just-published `lex-${TAG}-x86_64-unknown-linux-gnu.tar.gz` and
`lex-${TAG}-aarch64-unknown-linux-gnu.tar.gz` archives, verifies
their `.sha256` sidecars, runs `lex --version`, and smoke-tests
`lex check` on a tiny file. The aarch64 binary runs through
`qemu-user-static` so the check works on a stock x86_64 GitHub
runner.

This catches packaging regressions (a corrupted tarball, a
binary built for the wrong arch, a missing dynamic library) but
it doesn't replace on-hardware verification — qemu emulates the
ISA but not the real Jetson / Mac Studio environment. For
production-bound deploys, run the manual smoke test in the
"Pre-built release binaries" section above on the actual target
device before rolling forward.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `cross: error linking target: ...` | Missing target rustc component. `rustup target add <triple>`. |
| Docker daemon errors | `cross` needs the daemon. `systemctl start docker`. |
| Binary segfaults on Jetson | You downloaded the `x86_64-unknown-linux-gnu` artifact. Confirm with `file lex` — should say `aarch64`. |
| TLS errors on `agent.cloud_complete` | The cross image's `ca-certificates` may be stale. `cargo install cross --locked --version <newer>` or build natively. |
