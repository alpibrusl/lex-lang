# Hacker News — Show HN

## Title (≤ 80 chars)

```
Show HN: Lex – effect types for sandboxing code that LLMs write
```

### Alternates, same length budget

- `Show HN: Lex – a programming language for code no one will read`
- `Show HN: Lex – type-check rejection of LLM-emitted effects, before any code runs`

## Body

```text
Lex is a small functional language whose bet is that when AI agents
write more code than humans review, the function signature has to
become the contract.

Effects are part of the type. A function annotated

  fn fetch(url :: Str) -> [net] Result[Str, Str]

cannot reach the filesystem; if the body tries `io.read("/etc/passwd")`,
the type checker rejects the program before any byte runs. The
runtime then re-checks the policy at the dispatch site, so a function
declared `[fs_read("/data")]` granted at startup still has to pass
the path check at the actual read.

The motivating workflow is `lex agent-tool`: ask Claude/Codex/etc.
for a tool body, splice it into a fixed signature, run it under
--allow-effects. Anything outside the declared set is rejected at
type-check, not caught by a runtime exception. The adversarial bench
in the repo runs 7 attacks + 2 benign cases through three sandboxes:
Lex blocks 7/7 and runs 2/2; RestrictedPython blocks 3/7 and runs 2/2;
naive `exec` blocks 0/7. Reproduce with `cargo test -p lex-cli --test
agent_sandbox_bench`.

Other pieces that fell out of the same design: AST-native diff
(renames register as renamed, not delete+add), three-way structural
merge with JSON conflicts instead of <<<<<< markers, a content-
addressed stage store with `lex blame` for per-function history,
ACLI-compliant discovery so any LLM agent can drive the CLI without
a bespoke skill file, and a Spec sibling that emits SMT-LIB 2 for Z3.

Implementation: ~14 Rust crates, 285 tests passing, EUPL-1.2.

Limitations I'd want feedback on: the effect grammar is intentionally
small (io, net, fs_read, fs_write, time, rand, proc, budget, chat) and
adding new ones is a language change, not a library change — is that
the right call? And the static check can't catch "correct effect,
wrong intent" (e.g. `[net]` is granted but the body exfiltrates data) —
Spec proofs cover some of that, but the gap is real.

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

## Likely objections + prepared answers

| Objection | Short reply |
|---|---|
| "Just use Docker / V8 isolates / WASM" | Different layer. Containers gate at the syscall, Lex gates at the signature — the agent has to *declare* what it touches before code runs. Both can stack. (Linked section in `docs/index.html`.) |
| "Effect typing is just capabilities with extra steps" | Yes — Lex's contribution is that the capability is in the function signature, machine-checked, and surfaced in `lex audit --effect net` for review. |
| "A new language is a huge ask vs. a Rust crate" | Fair. The pitch isn't "rewrite your stack" — it's "the AI-emitted tool body lives in a 30-line Lex fragment under a known effect set." Rust calls into it via the HTTP API. |
| "Capability ≠ correctness" | Acknowledged in the post and on the landing page. Spec proofs (`lex spec check`) cover behavioral contracts; the gap between effect-correct and intent-correct is real. |
