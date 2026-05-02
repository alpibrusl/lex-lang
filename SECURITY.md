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
- **Stack overflow.** The parser caps recursion at `MAX_DEPTH=96`
  and the VM caps call-frame depth at `MAX_CALL_DEPTH=1024` —
  both refuse cleanly with structured errors instead of unwinding
  the host. Tail calls reuse frames, so productive
  tail-recursive code is unaffected. (Native-stack work the host
  performs *outside* a VM call — e.g. handling arbitrarily nested
  JSON in `lex parse` — is not bounded; for adversarial input
  layer container memory caps.)
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

## The `[proc]` effect: read this before you grant it

`[proc]` (subprocess spawn, `proc.spawn(cmd, args)`) is the
**escape hatch** in Lex's effect system. A function with `[proc]`
in its signature can do anything the spawned binary can do —
which, for a general-purpose tool like `git` or `bash`, is
*everything*. Once `[proc]` is in scope, the effect system stops
making strong claims about what the function does.

What `[proc]` **does** still gate:

- **Effect declaration** is mandatory. A function that calls
  `proc.spawn` without `[proc]` in its signature is rejected at
  type-check.
- **Binary basename allow-list** via `--allow-proc=git,gh,cargo`.
  An empty list means "any binary" (escape hatch — only for
  trusted code); a non-empty list rejects pre-spawn anything
  whose basename isn't on the list.
- **Per-arg length** capped at 64 KiB and **arg-count** capped at
  1024 — runtime DoS guards.
- **Type-checked argv shape**: `args :: List[Str]`. No
  shell-interpolation; the runtime calls `Command::new(cmd).args(args)`
  which passes argv directly to `execve`, no `/bin/sh -c` in the
  middle.

What `[proc]` **does not** gate:

- **Argument injection.** A binary that itself accepts an
  `--exec=...` or `-e ...` flag (think `bash -c`, `git -c
  alias.x=!sh`, `sed e ...`) can be subverted by attacker-controlled
  args. The allow-list says "you may run `git`"; it does not say
  "with these specific subcommands."
- **Environment variables.** The spawned process inherits the
  parent's env. A binary that reads `LD_PRELOAD`, `PATH`, or
  `GIT_*` vars can be redirected by manipulating those.
- **Working directory.** Inherits the parent's `cwd`. A binary
  that does `git status` runs against whatever repo your process
  is in.
- **Network, filesystem, signals.** Whatever the spawned binary
  does, the OS does. The Lex effect system doesn't follow
  execution into the child.

### Best practices when granting `[proc]`

1. **Always pass an explicit `--allow-proc` list.** Empty-list
   "escape hatch" mode is for one-off scripts you wrote, not for
   anything that runs LLM-emitted bodies.
2. **Allow-list narrowly.** `--allow-proc git,gh` is much
   narrower than `--allow-proc bash`. Avoid shells, interpreters,
   and any binary with a `--exec`-style flag.
3. **Validate argv at the Lex layer** before passing it to
   `proc.spawn`. If an attacker can inject into the args, the
   binary can do whatever its argv accepts.
4. **Layer with OS-level isolation.** Run the parent process in
   a container (`docker run --memory=...`), under gVisor, or
   with `nsjail` / `bubblewrap`. `[proc]` punches a hole in
   Lex's effect system; the container is what catches what
   falls through.
5. **Audit spawned-binary surfaces.** `lex audit --calls
   proc.spawn` finds every call site; pair with code review for
   the args.
6. **Treat `[proc]` as "I'm writing infrastructure, not running
   an agent."** Agent-emitted code that needs to call
   subprocesses should go through a Lex-side adapter that
   validates the cmd + args, not get `[proc]` directly.

We took the trade because `[proc]` unlocks real workflows
(orchestrating other CLI agents, running tests, talking to
`git`) that the existing effect set can't serve. The honesty
above is the price.

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
