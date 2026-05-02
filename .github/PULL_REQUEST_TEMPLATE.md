<!--
Thanks for contributing! See CONTRIBUTING.md for the conventions
this project follows. CI runs `cargo build`, `cargo test`, and
`cargo clippy --workspace --all-targets -- -D warnings`; please
make sure all three are green locally before pushing.
-->

## Summary

What does this PR change? One paragraph.

## Why

What's the user-facing problem this solves, or the gap this
closes? If it's tied to a tracked issue, link it (`closes #N`).

## Test plan

- [ ] `cargo test --workspace` passes locally
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] New behavior has a regression test
- [ ] If this changes a public CLI flag or JSON shape, README /
      Toolchain reference / langspec is updated

## Risk / scope

- Touches: `<crate-list-or-CLI-or-docs>`
- Breaking change to wire format / SigId / StageId / canonical AST? **yes / no**
- Adds a new effect kind or runtime variant? **yes / no**
