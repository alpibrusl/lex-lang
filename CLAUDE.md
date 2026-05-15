# CLAUDE.md — lex-lang (the toolchain itself)

> This is the source repo for the Lex language. The authoritative
> agent contract is `docs/AGENT_GUIDELINES.md` — the same content
> `lex agent-guidelines` prints. Downstream repos either copy that
> file as `AGENTS.md` or rely on the CLI. **In this repo, read
> `docs/AGENT_GUIDELINES.md` first**, then come back here for any
> repo-local overrides.

Lex is a typed-effect language with a content-addressed AST and an
attestation graph. The rules in `docs/AGENT_GUIDELINES.md` are the
project conventions for any Lex codebase; the ones below are extra
discipline that applies *to working on lex-lang itself*.

## Mandatory reading before writing code

Run these in order:

```sh
lex --version                  # confirm Lex is installed; if missing, see below
lex agent-guidelines           # authoritative idiom rules — read in full
lex skill                      # CLI surface + exit codes (ACLI)
```

`lex agent-guidelines` is the prescriptive contract for this project.
Do not write code until you have read it. The rules are numbered and
stable; this CLAUDE.md exists only to point you at them and add
project-specific overrides.

## The discipline summary

The full rules live in `lex agent-guidelines`. The four that matter
most when you're tempted to skip them:

1. **Narrow effects, always.** `fn foo() -> [fs_write("/tmp/x")] T`,
   not `[fs_write]`, not `[fs_write, fs_read, io]`. If the type checker
   rejects, narrow the **body**, not the signature.
2. **Repair, don't regenerate.** When `lex check` fails, run
   `lex --output json check` to get the structured error, then
   `lex repair --apply --transform '<suggested_transform>'`. Only
   regenerate after two failed repair attempts.
3. **`examples {}` blocks on every pure fn.** They're part of the
   SigId and run at `lex check` time — free regression tests with no
   `tests/` boilerplate.
4. **Use the stdlib.** `std.crypto` not hand-rolled crypto, `std.conc`
   not threads, `std.sql` not string-concat SQL, `std.regex` not
   manual scanners. Reach for raw bytes only after checking the
   stdlib index.

## The loop

Every change goes through the same four steps. **Do not claim a task
done before all four are green.**

```sh
lex check --strict src/        # type-check with extra lints
lex fmt --check src/ tests/    # formatting (must be canonical)
lex test                        # all tests/test_*.lex files
lex ci                          # umbrella: same as the above + pkg install
```

If `lex check` fails, do **not** broaden the effect signature to
make it pass. Investigate the body. See `lex agent-guidelines` § 1.2.

## When in doubt

```sh
lex agent-guidelines        # the rules
lex skill                   # the CLI surface
lex --output json check <file>   # structured errors with rule_tag + suggested_transform
lex blame <fn> --with-evidence   # what attestations already cover this fn
```

Lex toolchain version pinned by this project: see `lex.toml` /
`.github/workflows/lex.yml`. If `lex --version` reports a different
version locally, install the pinned one from
<https://github.com/alpibrusl/lex-lang/releases> before continuing.

## Project-specific overrides — lex-lang

- **Type-checker / runtime changes go through `cargo test` first.**
  The Rust crates under `crates/` are the compiler; `lex check`
  cannot be the gate here (the binary is what we're changing).
- **Builtin signatures live in `crates/lex-types/src/builtins.rs`.**
  Adding a new effect or stdlib fn means updating this file *and*
  `docs/AGENT.md`'s reference table.
- **`docs/AGENT_GUIDELINES.md` is the source of truth.** Don't fork
  the content into other docs; downstream repos should copy this
  file as `AGENTS.md`, not maintain a parallel one.
- **Release tags pin downstream CI.** Every downstream repo
  references a fixed `lex` version; bumping `Cargo.toml` here is a
  coordinated release, not a free edit.
