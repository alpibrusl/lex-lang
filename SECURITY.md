# Security Policy

This document describes Lex's security model: **what it is designed
to defend against, what it isn't, and how to deploy it safely**.

If you've found a vulnerability, see [Reporting](#reporting) below.

## What Lex defends against

Lex's threat model centers on **untrusted code emitted by an LLM
agent** (or a human you don't fully trust) running inside a process
you control.

Specifically, the type checker rejects, at compile time, any code
that:

1. **Performs an effect not in its declared signature.**
   `fn f() -> Str` cannot call `io.read`, `net.get`, or any other
   effectful operation. The signature *is* the contract.
2. **Calls a function with effects you haven't allow-listed.**
   With `--allow-effects net`, type-checking succeeds but a runtime
   policy gate rejects calls into `[fs_read]` / `[fs_write]` / `[io]`
   functions before they execute.
3. **Touches a file path or hostname outside your scope.**
   `--allow-fs-read /tmp` and `--allow-net-host api.example.com`
   bound the *targets* of `[fs_read]` and `[net]` operations, not
   just the kinds.
4. **Launders effects through higher-order code.**
   A closure's effect set is part of its type. Passing
   `fn() -> [io] Str` to a parameter typed `fn() -> Str` is a type
   error — unification fails on the empty-vs.-`[io]` mismatch.
   Stdlib HOFs (`list.map`, `list.fold`, ...) use effect-row
   variables so an effectful closure's effects propagate to the
   caller instead of being absorbed silently.

These guarantees are **mechanical**: they hold for any well-typed
program, including ones the author didn't intend to be safe. We've
verified this on 251+ tests including dedicated soundness tests
in `crates/lex-types/tests/`.

There's also an adversarial benchmark — `lex-lang/benches/` —
comparing `agent-tool` against a naive Python sandbox under
LLM-emitted attack payloads.

## What Lex does NOT defend against

Effect signatures bound *what kind* of access happens. They do not
bound:

- **Memory exhaustion.** `--max-steps` caps opcode dispatches
  (good for infinite loops); it does **not** cap memory allocations.
  A program declared `pure` can still allocate a list of 10^9 ints
  and OOM the host.
- **Stack overflow.** Deep recursion eventually overflows. There's
  no Lex-level recursion-depth limit.
- **CPU time.** `--max-steps` bounds dispatched ops; the per-op
  cost varies. Tight on its own is not a hard wall-clock bound.
- **Logic bugs that don't cross effect boundaries.** A `[net]`
  function calling the *wrong* `net.post` URL is still `[net]`.
  `--allow-net-host` narrows it; `lex spec` proves what the spec
  says (not what you meant).
- **Side channels.** Timing, cache, power, EM. Out of scope.
- **A compromised host.** If the OS is owned, the type checker
  is bypassed by definition.
- **Network adversaries on the wire.** `--allow-net-host` is
  allow-list scoping, not authentication. Use TLS (`net.serve_tls`,
  HTTPS in `net.get`/`net.post`) for transport security.
- **Spec correctness.** `lex spec` checks specs you write; it
  doesn't generate them. A spec that doesn't cover an input is not
  a Lex bug.

Treat Lex's effect system as **the innermost ring of defense in
depth**, not as a complete sandbox.

## Recommended deployment

For untrusted code paths (especially `lex agent-tool` and
`lex run` on agent-emitted input):

1. **Run inside a memory-limited container.**
   `docker run --memory=512m --pids-limit=128 ...`,
   `systemd-run --property=MemoryMax=512M ...`, or equivalent.
   This is the only reliable bound on memory and process count.
2. **Wrap with a wall-clock timeout.**
   `timeout 30s lex run ...`. `--max-steps` is complementary;
   neither replaces the other.
3. **Pin `--max-steps`.** The default is generous. For
   agent-emitted code, drop it to whatever covers your normal
   workload (10^5–10^6 typical).
4. **Prefer fine-grained scopes over kind-level allow-lists.**
   `--allow-fs-read /tmp/inputs` is much narrower than
   `--allow-effects fs_read`. Same for `--allow-net-host`.
5. **Layer with OS-level sandboxing for adversarial input.**
   gVisor, Firecracker, WebAssembly (`wasmtime`), nsjail. Lex
   narrows the attack surface; OS isolation contains residual
   risk (kernel exploits, side channels, resource exhaustion).
6. **Enable tracing for forensic replay.** `lex run --trace`
   records every effect call; `lex replay` reproduces the run with
   overrides. Useful both for debugging and post-incident review.

## What's in scope for security fixes

We treat the following as security bugs and fix them with priority:

- Type-checker false-negatives that admit code violating its
  declared effects (e.g. an effect leaks through a feature combination)
- Runtime-policy bypasses (e.g. `--allow-fs-read /a` allowing reads
  under `/b`)
- Capability gate bugs (effect declared but not enforced at runtime)
- Sandboxing escapes from `agent-tool` / `lex run` into the host

## What's not a security bug

- Denial of service via legitimate resource consumption (see above)
- Imperfect specs (`lex spec` proves what you write)
- Errors echoing user-supplied paths or hostnames back to the user
  (this is the input speaking, not a leak)
- Long-running spec-checker calls (Z3 is not always fast)

## Reporting

If you believe you've found a security vulnerability, **please do
not open a public GitHub issue**. Instead:

- Email the maintainers (see `Cargo.toml` `authors`), **or**
- Open a [GitHub Security Advisory](https://github.com/alpibrusl/lex-lang/security/advisories/new)

We aim to triage within a week. We don't currently run a bug-bounty
program.

## Versioning

Lex is pre-1.0. Breaking changes that close a soundness gap may
ship in any minor release; we'll call them out in `CHANGELOG.md`
and the release notes.
