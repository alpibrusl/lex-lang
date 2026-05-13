# Quickstart for AI agents — bootstrap a new Lex project

This doc is the **single URL** to point an AI assistant at when you
want it to implement something in Lex. It covers the path from an
empty directory to a green CI on GitHub. After step 2 the project
itself contains an `AGENTS.md` with the language-specific guidance —
this doc is intentionally only about getting there.

**Prompt fragment you can paste to your agent:**

> Implement \<project description\> in Lex. Bootstrap the project
> following <https://github.com/alpibrusl/lex-lang/blob/main/docs/QUICKSTART.md>.
> After running `lex init .`, follow the per-project `AGENTS.md` for
> language conventions. Do not claim done before `lex ci` is green.

---

## Prerequisites

- An empty directory under your control (typically a fresh `git init`).
- `curl`, `tar`, `git`, and a writable `/usr/local/bin` (or any directory
  on `PATH`).
- Network access to `github.com`.
- No Rust required.

## 1. Install the `lex` binary

Download the pre-built binary for your platform. There's no installer
yet; the binary is self-contained and goes anywhere on `PATH`.

```sh
LEX_VERSION=v0.9.1   # latest at the time of writing; check the Releases page
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)   TARGET=x86_64-unknown-linux-gnu  ;;
  Linux-aarch64)  TARGET=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64)  TARGET=x86_64-apple-darwin       ;;
  Darwin-arm64)   TARGET=aarch64-apple-darwin      ;;
  *) echo "unsupported platform" >&2; exit 1 ;;
esac
curl -sSfL "https://github.com/alpibrusl/lex-lang/releases/download/${LEX_VERSION}/lex-${LEX_VERSION}-${TARGET}.tar.gz" | tar -xz
sudo install -m 0755 "lex-${LEX_VERSION}-${TARGET}/lex" /usr/local/bin/lex
lex --version
```

Windows: grab `lex-v0.9.1-x86_64-pc-windows-msvc.zip` from
<https://github.com/alpibrusl/lex-lang/releases> and put `lex.exe`
on `PATH`.

Fallback (build from source) — only when you need an unreleased fix:

```sh
git clone --depth=1 https://github.com/alpibrusl/lex-lang /tmp/lex-lang
cd /tmp/lex-lang && cargo build --release -p lex-cli
export PATH="/tmp/lex-lang/target/release:$PATH"
```

## 2. Scaffold the project

From inside the empty directory:

```sh
git init                       # if you haven't already
lex init .
```

This drops five files, none overwriting anything that exists:

| File | Purpose |
|---|---|
| `lex.toml` | Package manifest (name, version, `[dependencies]`) |
| `src/main.lex` | Entry-point stub with an `examples { }` block |
| `tests/test_main.lex` | Test stub — `run_all` returns `0` for "all passed" |
| `.github/workflows/lex.yml` | GitHub Actions CI: pkg install, check, fmt, test, `lex ci` |
| `AGENTS.md` | Cold-start guide for AI assistants working in **this** project |

The CI workflow is pinned to the lex version you installed; `lex --version`
should match the `LEX_VERSION` env in the generated workflow.

## 3. Verify the baseline is green

```sh
lex ci
```

You should see `CI passed — all steps green`. If not, that's a bug in
the scaffold — file an issue on
<https://github.com/alpibrusl/lex-lang/issues> before continuing.

## 4. (Optional) Add dependencies

```sh
lex pkg add lex-schema --path ../lex-schema
# or:
lex pkg add lex-schema --git https://github.com/alpibrusl/lex-schema
lex pkg install
```

## 5. Implement the project

Open `src/main.lex` and `tests/test_main.lex` and start writing code.
The project's own `AGENTS.md` covers the language-specific bits an agent
needs to know — `::` for type annotations, effects-as-types, `Result`/
`Option` over exceptions, `examples { }` blocks, the `lex check → lex
test → lex fmt → lex ci` loop.

Iterate with:

```sh
lex check src/main.lex --output json    # structured errors with rule_tag + position
lex test                                # run tests/test_*.lex
lex fmt src/ tests/                     # auto-format
lex ci                                  # everything above, plus pkg install
```

`lex ci` is the gate — both contributors and CI run it, so a green
local `lex ci` is the same green that CI will report.

## 6. Commit and push

```sh
git add .
git commit -m "scaffold via lex init"
# Create the remote first (one-off):
gh repo create <owner>/<name> --public --source=. --remote=origin --push
# Or, if it already exists:
git remote add origin git@github.com:<owner>/<name>.git
git push -u origin main
```

GitHub Actions will pick up `.github/workflows/lex.yml` on the first
push and run `lex ci` against the latest commit.

## 7. Need more?

- `AGENTS.md` (inside the project you just scaffolded) — language
  cheat-sheet, project conventions.
- [`docs/AGENT.md`](AGENT.md) (upstream) — the deep reference: error
  envelope schema, every effect kind, stdlib module summary, sharp
  edges, repair-loop semantics.
- [`README.md`](../README.md) — design rules, contract-layer framing,
  what makes Lex different.

If something here is wrong or out of date, send a PR against this file:
<https://github.com/alpibrusl/lex-lang/blob/main/docs/QUICKSTART.md>.
