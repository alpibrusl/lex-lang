# `agent_sprint` — a sandboxed agent loop with effects as the
# security boundary

A four-stage pipeline that fetches a task, dispatches it to an LLM
CLI, verifies the result, and persists it. Every stage declares
exactly what it can do; the orchestrator's signature is the union;
the host's CLI policy is the entire perimeter.

The interesting bit is the threat model: there is **no process
sandbox** here. No bubblewrap, no seccomp, no Docker. The whole
demo runs unprivileged in the host shell. The thing that makes it
safe to run code an LLM emitted is that the type checker rejects
out-of-policy stages **before** they execute.

## Pipeline shape

```
            ┌─────────────┐    ┌──────────────┐    ┌────────┐    ┌──────────────┐
task.json ──► fetch_task  ├────► invoke_agent ├────► verify ├────►   persist    │
            │    [io]     │    │    [proc]    │    │ (pure) │    │ [kv, fs_write,
            └─────────────┘    └──────────────┘    └────────┘    │      log]    │
                                                                  └──────────────┘
```

Each box is a separate `.lex` file under `stages/`. The label
under each box is its declared effect set — the only thing that
stage can do at runtime. The orchestrator (`sprint.lex`) composes
them with `match`, and its own signature is `[io, proc, kv,
fs_write, log]` — exactly the union.

## Run it

The example task uses `echo` as the "agent" so the demo runs
without claude-code / cursor / etc. installed:

```sh
cd examples/agent_sprint
lex run sprint.lex sprint \
  '"./tasks/example.json"' '"echo"' '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     echo
```

Swap `"echo"` and `--allow-proc echo` for `"claude-code"` (or
`cursor-cli` / `gemini-cli` / `codex-cli`) once the agent CLI is
installed and on `$PATH`. The sprint script doesn't change — only
the host policy.

## Why this is interesting

### 1. The security perimeter is the policy line, not the source

Read the four invocation flags in the run command above. That's
the entire trust budget. A reviewer auditing this deployment
checks one place — the `lex run` invocation — to know what *any*
stage in this pipeline can possibly do. Compare to a process
sandbox where the analogous question is "what syscalls did
bubblewrap actually block, given my distro's config and the
agent's `unshare`-evasion attempts."

### 2. New stages can't smuggle capabilities

If someone adds `import "std.net" as net` and a `net.get(...)`
call into `verify.lex`, `lex check sprint.lex` fails:

    error: effect `net` not declared at <verify.run>
        verify.run's signature said pure, body wants [net]

The stage doesn't compile. No execution happens. Compare to a
process sandbox where the analogous bug is "the bwrap policy
forgot to deny outbound TCP, so the verifier is now exfiltrating."

### 3. Per-path / per-host scopes compose

Granting `[io]` doesn't grant unrestricted filesystem access —
`--allow-fs-read ./tasks` means *any* `io.read` call in *any*
stage can only see files under `./tasks/`. A prompt-injected
agent that emits `io.read("/etc/passwd")` fails at the policy
gate, on the line that tries to read, with a clear error.

### 4. The DAG is auditable by capability

```sh
lex audit --effect proc sprint.lex
```

…lists every stage that can spawn a subprocess. `lex audit
--effect io` lists every stage that touches the filesystem.
You read the pipeline by capability, not by trawling source.

### 5. Stages are content-addressed

`lex publish stages/invoke_agent.lex` records the stage's
canonical AST hash + effect signature in the store. `lex blame
stages/invoke_agent.lex` shows when the effects last changed —
which is when the trust budget last changed. That's the audit
log a reviewer actually wants.

## Adversarial walkthrough

To convince yourself, try the rejections live:

```sh
# 1. Try to invoke a binary not on the allow-list:
lex run sprint.lex sprint \
  '"./tasks/example.json"' '"rm"' '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     echo
# → Err("process.run: `rm` not in --allow-proc [\"echo\"]")

# 2. Edit verify.lex to add `import "std.io" as io` and an
#    io.read call, then `lex check sprint.lex`:
# → effect `io` not declared at <verify.run>

# 3. Remove `proc` from --allow-effects:
lex run sprint.lex sprint \
  '"./tasks/example.json"' '"echo"' '"./sprint.db"' \
  --allow-effects io,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     echo
# → effect `proc` not in --allow-effects (at invoke_agent.run, at sprint)

# 4. Path-scope refuses to read outside --allow-fs-read, even
#    though [io] is granted:
lex run sprint.lex sprint \
  '"/etc/passwd"' '"echo"' '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     echo
# → effect handler error: read of `/etc/passwd` outside --allow-fs-read
```

## Caveats called out

- **Sequential.** This v1 runs one stage at a time. Concurrent
  agents (`flow.race`, `flow.timeout`, `parallel_list` of
  effectful actions) need a true thread pool + `time.sleep` —
  language-level prereqs that aren't shipped yet. Tracked
  separately.
- **Single agent per sprint.** A multi-agent dispatcher that
  abstracts over claude-code / cursor / gemini calling
  conventions is one more file (~30 lines per agent). Skipped
  here to keep the demo focused on the security pitch.
- **No git / Gitea integration.** Caloron-Noether (the project
  this demo is inspired by) wires the sprint loop into a Gitea
  issue tracker. That belongs in a `std.git` module wrapping
  `std.http`; out of scope for the demo.

## Layout

```
examples/agent_sprint/
├── README.md            ← you are here
├── sprint.lex           ← orchestrator (entry)
├── types.lex            ← shared `Task` type
├── stages/
│   ├── fetch_task.lex   ← [io]
│   ├── invoke_agent.lex ← [proc]
│   ├── verify.lex       ← pure
│   └── persist.lex      ← [kv, fs_write, log]
└── tasks/
    └── example.json     ← sample input
```
