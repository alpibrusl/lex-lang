# Contributing to lex-lang

Thanks for considering a contribution. Lex is pre-1.0 — the
roadmap is opinionated and the test bar is high, but we welcome
small fixes, fresh examples, and well-scoped features.

## Quick start

```bash
git clone https://github.com/alpibrusl/lex-lang
cd lex-lang
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

CI on every PR runs the same three steps. Anything that doesn't pass
locally won't pass in CI; please don't push known-red branches.

## Branch naming

Branches that touch shipped code use the `claude/<short-slug>` prefix
(legacy naming from when most contributions came through agents). New
human contributions can use `<github-handle>/<short-slug>`. PRs on
either pattern are accepted.

## What changes are easy to land

- **Bug fixes** with a regression test that fails before the fix and
  passes after. The smaller the diff, the faster the review.
- **New stdlib operations** in pure modules (`std.str` / `std.list` /
  `std.option` / etc.) — see `crates/lex-runtime/src/builtins.rs`.
  Add the type signature in `crates/lex-types/src/builtins.rs` and a
  test in `crates/lex-runtime/tests/`.
- **Example apps** under `examples/` that exercise a real workflow.
  Pair with an integration test in `crates/lex-runtime/tests/`.
- **Doc updates**, especially the README's status table and the
  langspec when behavior changes.

## What needs design discussion first

Open an issue (with the `discussion` label) before writing code if
your change:

- Adds a new effect kind (`io` / `net` / etc.) — touches the policy
  gate and the agent contract.
- Changes the canonical AST or hashing — `SigId` / `StageId`
  identity is a wire format.
- Changes the runtime `Value` enum — every `match v` site has to
  grow a new arm.
- Touches the spec checker's proof obligations.

## Style + invariants

- **No new `unwrap`/`expect`** in production paths. Tests and
  hand-rolled internals can use them when failure means a programmer
  bug, not user input. Errors at boundaries flow through `Result`.
- **Tests live alongside the crate they exercise** —
  `crates/<crate>/tests/<name>.rs`. End-to-end CLI tests live in
  `crates/lex-cli/tests/`.
- **Comments explain WHY, not WHAT.** A line that's surprising,
  load-bearing, or contradicts the surrounding pattern earns a
  comment; a line that does what it says doesn't.
- **No backwards-compatibility shims** for unused features.
- **No throwaway feature flags** for one-off rollouts.

## Submitting a pull request

1. Make a branch (see naming above).
2. Push commits — small, atomic, with `<area>: <imperative summary>`
   first lines (e.g. `parser: cap recursion depth at 96`).
3. Open the PR. Use the template in `.github/PULL_REQUEST_TEMPLATE.md`.
4. CI runs build + test + clippy + fuzz on parser/AST/type-checker
   touches. All must be green.
5. Review happens on the PR. Push fixes as new commits — squash
   happens at merge time.

## Security issues

**Don't open a public issue.** See [`SECURITY.md`](SECURITY.md) for
the disclosure path.

## Licensing

Lex is [EUPL-1.2](LICENSE). Contributions are under the same license.
By submitting a PR you agree to license your changes under EUPL-1.2.

## Code of conduct

We follow the [Contributor Covenant 2.1](CODE_OF_CONDUCT.md). Disrespect
toward other contributors gets you uninvited, regardless of the merit
of your code.
