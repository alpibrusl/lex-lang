# Recording the `[proc]` agent-dispatcher demo

The third recording, alongside `RECORDING.md` (agent-tool security)
and `RECORDING_VC.md` (agent-native VC). This one demos the
**escape-hatch effect**: `[proc]` is the only effect in Lex that
can spawn arbitrary binaries, and we're explicit about how the
allow-list contains it.

## What you need

- `asciinema` installed.
- The release-mode binary (`cargo build --release`).
- A clean shell.

## Pre-recording setup

```bash
export PATH="$(pwd)/target/release:$PATH"
stty cols 100 rows 30
clear
```

## Recording flow

```bash
asciinema rec bench/agent_dispatcher_demo.cast \
  --idle-time-limit 1.5 \
  --title "Lex [proc]: agent dispatcher with binary allow-list, type-checked argv"
```

Inside the session, the **four beats** below. Each is a punchline.

### Beat 1 — The contract

```bash
# 1. The function declares [proc] in its signature. Read the SECURITY
#    note in the same file before granting [proc] in production.
head -30 examples/agent_dispatcher.lex
```

> **Punchline**: `fn run(cmd, args) -> [proc] Output` says exactly
> what the function can do. The type checker enforces it.

### Beat 2 — Allowed: spawning `echo`

```bash
# 2. Grant [proc], allow the `echo` binary, run a benign command.
lex run --allow-effects proc --allow-proc echo \
  examples/agent_dispatcher.lex run '"echo"' '["hello","from","agent_dispatcher"]'
# → {"exit_code":0,"ok":true,"stderr":"","stdout":"hello from agent_dispatcher\n"}
```

> **Punchline**: clean Output record, exit code 0.

### Beat 3 — Blocked: trying to spawn `rm` with the same policy

```bash
# 3. Same policy. Ask to spawn `rm` instead — runtime gate blocks it
#    pre-spawn, no destructive call ever happens.
lex run --allow-effects proc --allow-proc echo \
  examples/agent_dispatcher.lex run '"rm"' '["-rf","/tmp/anything"]'
# → {"exit_code":-1,"ok":false,"stderr":"proc.spawn: `rm` not in --allow-proc [\"echo\"]","stdout":""}
```

> **Punchline**: `--allow-proc` is the line of defense. Without it
> on the allow-list, `rm` doesn't run — the runtime returns an Err
> as a value, not a thrown exception.

### Beat 4 — The honesty: `lex audit` finds every `[proc]` call site

```bash
# 4. Before you grant [proc] in production, find everywhere it's
#    used and review the args. lex audit makes this trivial.
lex audit --effect proc examples/
# → SUMMARY: ... 1 stages
#   examples/agent_dispatcher.lex::run
#     fn run(cmd :: Str, args :: List[Str]) -> [proc] Output
#   examples/agent_dispatcher.lex::dispatch_all
#     fn dispatch_all(jobs :: List[Tuple[Str, List[Str]]]) -> [proc] List[Output]
```

> **Punchline**: `[proc]` is the escape hatch — `lex audit` is how
> you keep tabs on it. Read SECURITY.md before granting.

### End the session

`Ctrl-D` (or `exit`). Cast file is small.

## Sharing

```bash
asciinema upload bench/agent_dispatcher_demo.cast
agg bench/agent_dispatcher_demo.cast bench/agent_dispatcher_demo.gif \
  --theme monokai --font-size 14 --speed 1.5
```

## Tips for the take

- Beat 1 is just a `head -30`; pause after it so the viewer reads
  the signature + the SECURITY note in the file's header.
- Beat 3 is the money shot — make sure the JSON output is on
  screen long enough to read.
- Don't grant `--allow-proc bash` or `--allow-proc sh` in any
  recording. The point is *narrow* allow-lists.

## Reproducibility

The four commands are idempotent and need no external state. Any
contributor can re-record by running them.
