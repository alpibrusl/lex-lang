# Hacker News — Show HN

## Title (≤ 80 chars)

```
Show HN: Lex – effect types for sandboxing code that LLMs write
```

### Alternate

- `Show HN: Lex – type-check rejection of LLM-emitted effects, before any code runs`

## Body

```text
I've been working on Lex, a small functional language. The
thing I kept wanting when running agent-generated code locally
was a way to say "this body can touch the network and nothing
else" and have the type checker actually enforce it, not catch
it as a runtime exception after something already escaped.

In Lex, effects are part of the type. A function annotated

  fn fetch(url :: Str) -> [net] Result[Str, Str]

cannot reach the filesystem; if the body tries
`io.read("/etc/passwd")`, the program is rejected at type-check
before it runs. The runtime then re-checks the policy at the
dispatch site, so a function declared `[fs_read("/data")]` and
granted at startup still has to pass the path check at the
actual read.

To check whether the idea held, I wrote `lex agent-tool`: ask
Claude/Codex/etc. for a tool body, splice it into a fixed
signature, run it under --allow-effects. Anything outside the
declared set is rejected at type-check.

The adversarial bench in the repo runs 7 attacks + 2 benign
cases through three sandboxes — Lex blocks 7/7 and runs 2/2;
RestrictedPython blocks 3/7 and runs 2/2; naive `exec` blocks
0/7. The honest read is that the difference isn't cleverer
rules — it's where the rejection happens. RestrictedPython
rejects at runtime after starting; Lex rejects at type-check
before running. I picked the attacks myself, so take the
numbers with that grain of salt; reproduce with
`cargo test -p lex-cli --test agent_sandbox_bench`.

Other pieces that fell out of the same design: AST-native diff
(renames register as renamed, not delete+add), three-way
structural merge with JSON conflicts instead of <<<<<< markers,
a content-addressed stage store with `lex blame` for per-
function history, ACLI-compliant CLI discovery so any LLM
agent can drive it without a bespoke skill file, and a Spec
sibling that emits SMT-LIB 2 for Z3.

Implementation: ~14 Rust crates, 285 tests, EUPL-1.2.

Two things I'd genuinely like feedback on:

1. The effect grammar is closed (io, net, fs_read, fs_write,
   time, rand, proc, budget, chat). Adding one is a language
   change. I went with closed because the runtime has to know
   what to enforce — a user-defined effect that the runtime
   treats as opaque is a silent capability leak. But it pushes
   "domain effects" (db, email, …) onto host-level policy
   rather than the type system, which feels like a real cost.
   Is closed the right call?

2. The static check can't catch "correct effect, wrong intent"
   — a `[net]`-granted body can still exfiltrate. Spec proofs
   cover part of that gap. The rest is unclaimed and I don't
   have a great answer for it yet.

Repo: https://github.com/alpibrusl/lex-lang
Landing page: https://alpibrusl.github.io/lex-lang/
```

## Posting notes

- **Window:** Tue–Thu, 8–10am Pacific is the strongest slot for
  technical Show HNs. Avoid Fridays and holidays.
- **First comment from you (the author):** post one immediately
  with the asciinema cast (or GIF) from `bench/RECORDING.md`. HN
  rewards a 60-second demo.
- **Engage replies fast.** First-hour engagement drives ranking.
  Have the repo + landing page open and reply to objections with
  links to specific files / tests, not marketing copy.
- **Don't edit the title after posting.** HN resets the rank when
  you do.
- **Don't link to Twitter / paywalled press.** Direct repo +
  landing page only.
- **Verify before posting:**
  - GitHub Pages URL resolves (`alpibrusl.github.io/lex-lang/`).
  - `cargo test -p lex-cli --test agent_sandbox_bench` passes on
    a clean clone.
  - Test count badge in `README.md` matches reality.

## Likely objections + honest answers

| Objection | Reply |
|---|---|
| "Just use Docker / V8 isolates / WASM" | Different layer. Containers gate at the syscall, Lex gates at the signature — the agent has to *declare* what it touches before code runs. Both can stack; Lex is meant to ride on top of, not replace, OS-level sandboxing. |
| "Effect typing is just capabilities with extra steps" | Fair. The contribution Lex is trying to make is keeping the capability in the function signature, machine-checked, and surfaced for review (`lex audit --effect net`). Whether that's worth a new language is a fair question. |
| "A new language is a huge ask vs. a Rust crate" | Yes. The pitch isn't "rewrite your stack" — it's "the AI-emitted tool body lives in a 30-line Lex fragment under a known effect set." Rust calls into it via the HTTP API. If that framing doesn't help, this probably isn't for you. |
| "Capability ≠ correctness" | True, and I should say it before someone else does. Spec proofs cover behavioral contracts; the gap between effect-correct and intent-correct is real and unsolved. |
