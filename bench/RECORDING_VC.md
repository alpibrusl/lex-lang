# Recording the agent-native VC demo

The companion to `RECORDING.md` (which covers `lex agent-tool` —
the type-check-rejects-malicious-LLM-code arc). This one walks
through the **agent-native version control** story: branches,
structural merge with JSON conflicts, per-fn history, and
LLM-agnostic discovery via ACLI.

The two recordings are designed to live side-by-side on the
landing page — `agent-tool` is the security pitch, this one is
the workflow pitch.

## What you need

- `asciinema` installed (`brew install asciinema` /
  `pip install asciinema` / `apt install asciinema`).
- The release-mode binary: `cargo build --release` so output
  isn't cluttered with `Compiling …` lines.
- `jq` for the discovery teaser (most systems already have it).
- A clean shell — no leaked `LEX_STORE` env var.

## Pre-recording setup

```bash
# Clean room: dedicated store path so the demo runs idempotently.
export LEX_STORE=/tmp/lex_demo_store
rm -rf "$LEX_STORE"
export PATH="$(pwd)/target/release:$PATH"

# Pre-create the demo files so we don't waste recording time
# typing them. The recording will only run lex commands.
mkdir -p /tmp/lex_demo
cat > /tmp/lex_demo/v1.lex <<'EOF'
import "std.net" as net

fn fetch(url :: Str) -> [net] Str {
  match net.get(url) {
    Ok(s) => s,
    Err(e) => "fetch failed",
  }
}
EOF

cat > /tmp/lex_demo/v2.lex <<'EOF'
import "std.net" as net
import "std.io" as io

fn fetch(url :: Str) -> [net, fs_read] Str {
  match net.get(url) {
    Ok(s) => match io.read("/etc/passwd") {
      Ok(p) => s,
      Err(e) => s,
    },
    Err(e) => "fetch failed",
  }
}
EOF

# Terminal hygiene.
stty cols 100 rows 30
clear
```

## Recording flow

```bash
asciinema rec bench/agent_vc_demo.cast \
  --idle-time-limit 1.5 \
  --title "Lex agent-native VC: structural merge, per-fn history, LLM-agnostic discovery"
```

Inside the recording, run the eight commands below in order.
Each one has a single-sentence "punchline" — pause briefly after
each so the viewer's eye can land before you type the next.

### Beat 1 — LLM-agnostic discovery (the opener)

```bash
# 1. Any agent can read the surface — Claude, Codex, Gemini, Qwen, Mistral.
lex --output json introspect | jq '.data.commands | length'
# → 20

lex --output json introspect | jq -r '.data.commands[].name' | head
# → parse / check / run / hash / blame / publish / store / trace / replay / diff
```

> **Punchline**: `lex skill > LEX.md` drops a self-describing manifest
> any LLM can pick up. No bespoke skill file per agent platform.

### Beat 2 — Static safety (security primer)

```bash
# 2. The signature is the contract: this body declares [net] but
#    reaches into the filesystem. Type-check rejects.
lex check /tmp/lex_demo/v2.lex
# → {"kind":"undeclared_effect","effect":"fs_read","at":"fetch"} ; exit 2
```

> **Punchline**: this verdict happens before a single byte of the
> body runs. The runtime never even compiles it.

### Beat 3 — Effect-aware diff

```bash
# 3. ast-diff isolates effect changes — security-relevant, called out
#    explicitly so reviewers (human or agent) don't have to re-parse
#    a rendered signature.
lex ast-diff /tmp/lex_demo/v1.lex /tmp/lex_demo/v2.lex
# → ~ modified fn fetch(url :: Str) -> [net] Str
#                 → fn fetch(url :: Str) -> [net, fs_read("/etc/passwd")] Str
#               ⚠ effects gained: [fs_read("/etc/passwd")]
```

> **Punchline**: the **⚠ effects gained** line is the audit signal.

### Beat 4 — Branches + structural merge

```bash
# 4a. Publish v1 to main as the baseline.
lex publish --activate /tmp/lex_demo/v1.lex

# 4b. Branch off, publish v2 to feature.
lex branch create feature
lex branch use feature
lex publish --activate /tmp/lex_demo/v2.lex

# 4c. Now imagine main moved underneath us. Switch back, publish a
#     different change to the same fn so the merge has a real conflict.
lex branch use main
cat > /tmp/lex_demo/main_v2.lex <<'EOF'
import "std.net" as net

fn fetch(url :: Str) -> [net] Str {
  match net.get(url) {
    Ok(s) => match str.to_upper(s) {
      _ => s,
    },
    Err(e) => "fetch failed",
  }
}
EOF
# (Skip the publish here for the recording; pretend main was edited
# directly so the merge surfaces a conflict.)

# 4d. Merge feature → main: conflict surfaces as JSON, not <<<<<HEAD.
lex --output json store-merge feature main | jq '.conflicts[] | {kind, sig_id}'
# → {"kind": "modify-modify", "sig_id": "..."}
```

> **Punchline**: agents resolving the conflict get **structured JSON**,
> not a re-parse of a corrupted file. No `<<<<<<< HEAD` markers.

### Beat 5 — Clean merge + journal

```bash
# 5a. Resolve by accepting feature's version (just re-publish it on main).
lex publish --activate /tmp/lex_demo/v2.lex
lex branch use main
lex store-merge feature main --commit
# → → committed merge into `main` (1 fn)

# 5b. lex log shows the journal of merges committed into main.
lex log main
# → main: 1 merge(s)
#     • feature → main    1 fns @ 2026-05-01T20:00Z
```

> **Punchline**: `lex log` is the agent-VC equivalent of `git log`,
> but per-branch and rooted in stage identity, not text-line diff.

### Beat 6 — Per-fn history

```bash
# 6. lex blame shows what's in the store for every fn in the source —
#    current StageId, what's Active, full predecessor sequence with
#    statuses + timestamps. The current source's stage is marked '←'.
lex blame /tmp/lex_demo/v2.lex
# → fn fetch
#     sig:     ...…
#     current: ...…  (active)
#     history: 2 stage(s)
#       <prev>...  deprecated 2026-05-01T19:55Z
#       <curr>...  active     2026-05-01T20:00Z ←
```

> **Punchline**: per-fn provenance, not per-file. Renaming `fetch`
> doesn't lose its history; structurally it's the same SigId.

### End the session

`Ctrl-D` (or `exit`). The cast file is small (~30–60 KB).

## Sharing

```bash
# Upload to asciinema.org.
asciinema upload bench/agent_vc_demo.cast
# → returns https://asciinema.org/a/<id>

# Or convert to GIF for README / Twitter / LinkedIn.
agg bench/agent_vc_demo.cast bench/agent_vc_demo.gif \
  --theme monokai --font-size 14 --speed 1.5
```

For HN, embed the asciinema iframe near the top of `docs/index.html`,
with a `agent-tool` and an `agent_vc` toggle.

## Tips for the take

- Keep the terminal at 100×30 — wider lines wrap on phone screens.
- Pre-clear with `clear` so the recording starts on a blank prompt.
- If a `lex` invocation pauses (e.g. `lex publish` writes a few JSON
  files), narrate it briefly before the next command.
- Beat 5's merge step assumes the conflict is resolvable by taking
  one side; if you want the demo to show the *body-level* conflict
  JSON instead, replace step 4d's `--output json` with the
  human-readable `lex store-merge feature main` and read the conflict
  out loud.

## Reproducibility note

These commands are idempotent against the `LEX_STORE` we set up.
Future contributors can rerun the entire script and get a comparable
recording (with their own timestamps in the log/blame output).
