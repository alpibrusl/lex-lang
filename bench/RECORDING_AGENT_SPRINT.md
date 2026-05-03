# Recording the `agent_sprint` security-perimeter demo

The fourth recording in the series, alongside [`RECORDING.md`](RECORDING.md)
(agent-tool security), [`RECORDING_VC.md`](RECORDING_VC.md) (agent-
native VC), and [`RECORDING_PROC.md`](RECORDING_PROC.md) (`[proc]`
escape hatch). This one shows a **multi-stage pipeline** where each
stage's effect signature contributes to the orchestrator's perimeter,
and the host policy is the entire trust budget.

The story is the inverse of `RECORDING.md`: instead of one agent-
emitted body type-checking against a single declared effect, this
shows a four-stage DAG where every stage has its own narrower
contract and the verdict is a real runtime outcome (PASS / FAIL),
not just compile-time.

## What you need

- `asciinema` installed.
- The release-mode binary: `cargo build --release` so output isn't
  cluttered with `Compiling …` lines.
- The lex binary on `$PATH` so `which lex` resolves (the dogfood
  beat below uses `lex` as the agent CLI).

The recording does **not** need an `ANTHROPIC_API_KEY` — we use
`lex` itself as the agent, which makes the take deterministic and
reproducible. Anyone re-running the script gets the same transcript.

## Pre-recording setup

```bash
cargo build --release
export PATH="$(pwd)/target/release:$PATH"
which lex                 # confirm release binary is found
cd examples/agent_sprint
stty cols 100 rows 30
clear
```

## Recording flow

```bash
asciinema rec ../../bench/agent_sprint_demo.cast \
  --idle-time-limit 1.5 \
  --title "Lex agent_sprint: effect types as the security perimeter"
```

Inside the session, the **four beats** below. Each is a punchline.
Read each "Punchline" line out loud to yourself before typing the
command — that's the slide title for that frame of the recording.

### Beat 1 — The pipeline shape

```bash
# 1. Four stages, four files, four effect sets. The orchestrator
#    composes them; its signature is the union.
ls stages/
cat sprint.lex
```

> **Punchline**: every stage has its own effect signature. Read the
> directory tree and you've read the security model.

### Beat 2 — Static check + happy dogfood run

```bash
# 2a. The type checker reports exactly which effects the host needs
#     to grant.
lex check sprint.lex

# 2b. Run the dogfood task. The "agent" is lex itself, type-checking
#     the verifier's source. PASS because lex check prints "ok" on
#     a clean type-check.
lex run sprint.lex sprint \
  '"./tasks/dogfood.json"' "$(printf '%q' "$(which lex)")" '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     lex
# → Ok("dogfood-check-verify-stage => PASS")
```

> **Punchline**: `lex check` told us the exact policy. We supplied
> exactly that, and the language type-checked itself end-to-end.

### Beat 3 — Failing input, verifier rejects

```bash
# 3. Same pipeline, different task: lex check on a non-Lex file.
#    stdout is empty, "ok" not contained, verdict flips to FAIL.
lex run sprint.lex sprint \
  '"./tasks/broken.json"' "$(printf '%q' "$(which lex)")" '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     lex
# → Ok("dogfood-check-nonexistent => FAIL")
```

> **Punchline**: the agent stage *always* returns Ok for a
> successfully-spawned process. "Agent ran" vs. "agent ran
> *correctly*" is the verifier's job, by signature.

### Beat 4 — Adversarial: host policy + type checker

Three rejections in quick succession. Type at the same pace as the
happy run so the rhythm makes the contrast land.

```bash
# 4a. Try to invoke a binary not on --allow-proc. Runtime gate
#     refuses; the orchestrator catches the error.
lex run sprint.lex sprint \
  '"./tasks/dogfood.json"' '"rm"' '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     lex
# → Err("process.run: `rm` not in --allow-proc [\"lex\"]")

# 4b. Read a path outside --allow-fs-read, even though [io] is
#     granted. Per-path scope wins.
lex run sprint.lex sprint \
  '"/etc/passwd"' "$(printf '%q' "$(which lex)")" '"./sprint.db"' \
  --allow-effects io,proc,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     lex
# → effect handler error: read of `/etc/passwd` outside --allow-fs-read

# 4c. Drop `proc` from --allow-effects entirely. Static rejection
#     before *any* stage executes — note the {"kind":"effect_not_allowed"}
#     output happens before the "info: sprint starting" log line.
lex run sprint.lex sprint \
  '"./tasks/dogfood.json"' "$(printf '%q' "$(which lex)")" '"./sprint.db"' \
  --allow-effects io,kv,fs_write,log \
  --allow-fs-read  ./tasks \
  --allow-fs-write ./sprint.db \
  --allow-proc     lex
# → effect_not_allowed at invoke_agent.run, at sprint
```

> **Punchline**: three different rejection points — the runtime
> handler, the path scope, and the static type checker — all from
> the same source program by changing only the host policy line.
> No process sandbox involved.

End the session with `Ctrl-D` (or `exit`). The cast file is small
(~12–25 KB).

## Sharing

```bash
# Upload to asciinema.org for an embeddable player.
asciinema upload bench/agent_sprint_demo.cast
# → returns https://asciinema.org/a/<id>

# Or convert to a GIF for the README / Twitter / LinkedIn.
brew install agg          # https://github.com/asciinema/agg
agg bench/agent_sprint_demo.cast bench/agent_sprint_demo.gif \
  --theme monokai --font-size 14 --speed 1.5
```

Drop the URL or GIF into the README under "Sandboxing agent-
generated code", next to the `agent-tool` recording. The two
together tell a complete story: `agent-tool` is the
**single-function** case (one body, one effect contract);
`agent_sprint` is the **multi-stage** case (four files, four
contracts, one perimeter).

## Tips for the take

- Run the four happy / failing / adversarial cases **once** before
  recording so `cargo build --release` is hot and there's no
  mid-take spinner. Delete `examples/agent_sprint/sprint.db`
  between rehearsal and the real take so the recording's first
  `kv.open` is fresh.
- Make the terminal a clean 100×30 (`stty cols 100 rows 30`) and
  `clear` before starting.
- Type at human speed; `--idle-time-limit 1.5` collapses awkward
  pauses on playback.
- The release binary is much faster than debug — total recording
  is ~90s end-to-end. Don't speed up the agg conversion past 1.5×
  or beat 4's three rejections blur into one frame.

## Reproducibility note

Unlike [`RECORDING.md`](RECORDING.md), which uses a real LLM and
gets non-byte-identical takes per run, **this recording is fully
deterministic**: `lex check` produces the same stdout for the same
input, every time. Anyone with a checkout can reproduce the exact
transcript by following the four beats above. The `sprint.db`
artifact is the only side-effect; `rm -rf examples/agent_sprint/sprint.db`
between takes resets it.

## Why a separate recording (and not just an extension of `RECORDING.md`)

`RECORDING.md` shows the **point case** — a single agent-emitted
body, a single declared effect, the type checker rejecting one
escape attempt. That's the easy version of the pitch.

`agent_sprint` is the **composed case** — four stages, four
narrower contracts, the orchestrator's perimeter as the *union*.
This is what production agent loops actually look like, and it's
where you can see capabilities that don't exist in the point case:
per-stage audit (`lex audit --effect proc sprint.lex`), `lex
blame stages/invoke_agent.lex` showing when the trust budget
last changed, and the verifier as a **pure function** that can't
do anything observable even if its inputs are hostile.

The two recordings together answer the two questions a security-
minded reviewer asks: "can a single function be sandboxed?"
(yes — `RECORDING.md`) and "does the sandbox compose across a
real pipeline?" (yes — this one).
