# Reddit

Reddit fragments by sub. Pick one or two — don't blast all four
the same day, the mod queues notice cross-posts and people downvote
them.

## /r/rust

Audience: Rust developers. Lead with the implementation, not the
language pitch.

### Title

```
Lex: a small functional language in ~14 Rust crates with effect-typed sandboxing for LLM-emitted tool bodies
```

### Body

```text
Hi r/rust — sharing a project I've been building.

Lex is a functional language whose type system encodes effects in
function signatures (`fn f(...) -> [net] Result[Str, Str]`). The
implementation is a Rust workspace: lex-syntax (logos lexer + a
hand-written parser), lex-ast (canonical AST + structural diff/
merge), lex-types (the effect-aware type checker), lex-bytecode
(stack-based VM), lex-runtime (effect handlers — gated tiny_http
server, ureq client, tungstenite WS), lex-store (content-
addressed sig/stage store), lex-cli (the `lex` binary), plus a
Core sibling for sized numerics + tensor shape arithmetic.

A few Rust-flavored things that turned out useful:

- **logos for the lexer.** ~70 tokens, derives the DFA. The
  performance is good enough that the parser is the bottleneck,
  not the lexer.
- **rustls + platform-verifier via ureq 3.x** for the HTTP
  client. Native cert store, no native-tls dependency.
- **tungstenite 0.29 for the WS chat builtin.** Per-connection
  worker thread + mpsc::channel for outbound; the room registry
  is `Arc<Mutex<IndexMap<...>>>`. Lex code stays pure; the
  shared state lives in the host runtime.
- **The whole CLI is ACLI-spec compliant** (`lex --output json
  introspect`), so any LLM agent can drive it without a bespoke
  skill file. The `.cli/commands.json` is generated and committed
  for agents browsing the repo.

The bit I'm proudest of and would love feedback on: the
effect-checker's error messages thread the path of effect
inheritance through `flow.sequential` / `flow.branch` so that
when a higher-order combinator's body has an unexpected effect,
the error points to the inner lambda, not the combinator call
site. It's not novel research — just a lot of careful spans —
but the output is legible.

285 tests, EUPL-1.2, MSRV 1.80.

Repo: https://github.com/alpibrusl/lex-lang
```

### Notes

- Don't crosspost to /r/programming the same day.
- /r/rust mods sometimes redirect language-pitch posts to a
  weekly thread. If that happens, repost in the thread and
  link there from your other launches.

---

## /r/ProgrammingLanguages

Audience: PL nerds. Lead with design choices and tradeoffs.

### Title

```
Lex: row-based effects in a small functional language designed for LLM-emitted code
```

### Body

```text
I've been building a small functional language with effects in
the function signature — not algebraic effects (no handlers in
user code), just a closed row of capability tags that propagate
through application and pattern-match.

    fn fetch(url :: Str) -> [net] Result[Str, Str]
    fn echo(line :: Str) -> [io] Nil

Type rule: a function declared `[E]` can only call functions
whose effect set is a subset of `E ∪ {pure}`. Subtyping is
row-based with subset; no row variables in user syntax (the
internal representation has them, the surface doesn't).

Three design choices I'd genuinely like to argue about:

1. **Closed effect grammar.** io, net, fs_read, fs_write, time,
   rand, proc, budget, chat. Adding one is a language change.
   I picked closed because the runtime has to know what to
   enforce; user-defined effects that the runtime treats as
   opaque are silent capability leaks. The cost is that
   "domain effect" use cases (e.g. `[db]`, `[email]`) have to
   ride on top of one of the existing tags + a host-level
   policy.

2. **No effect polymorphism in user syntax.** Functions can be
   parameterized over types but not effect rows. This makes the
   error messages much better but means combinators like
   `flow.sequential` are monomorphized per effect set in the
   stdlib. Fair tradeoff?

3. **Effects as part of the canonical AST hash.** Two functions
   with the same body but different effect annotations get
   different StageIds in the content-addressed store. This is
   the basis for `lex blame` — per-fn lifecycle history that
   tracks effect changes as separate stages.

The motivating workflow is sandboxing LLM-emitted tool bodies:
the host declares `--allow-effects net` and any body that
reaches outside is rejected at type-check, before execution.
Adversarial bench (7 attacks + 2 benign) at bench/REPORT.md
compares against RestrictedPython.

Spec sibling adds randomized property checking + SMT-LIB 2
export to Z3 for behavioral contracts (capability ≠ correctness).

Repo: https://github.com/alpibrusl/lex-lang
```

### Notes

- /r/ProgrammingLanguages reads carefully and asks substantive
  questions. Budget at least a couple hours after posting.
- Be ready to reply about Koka / Eff / Frank / OCaml 5 effect
  handlers. The honest answer is "Lex is a deliberately
  smaller subset."

---

## /r/programming

Audience: broad. Lead with the practical problem.

### Title

```
A programming language designed for code no one will read
```

### Body

```text
Watching agents emit tool bodies, I kept wanting a way for the
host to say "this code can touch the network and nothing else"
that the type checker would actually enforce. Lex is what fell
out of that.

Effects are part of the type:

    fn fetch(url :: Str) -> [net] Result[Str, Str]

If the body of fetch tries to read a file, the type checker
rejects the whole program before any byte runs. The runtime
also re-checks the policy at the dispatch site, so a function
declared with `[fs_read("/data")]` and granted at startup still
has to pass the path check at the actual read.

I ran an adversarial bench: 7 attacks + 2 benign cases against
naive Python exec, RestrictedPython, and Lex. Lex blocks 7/7
and runs 2/2 (rejection happens at type-check). RestrictedPython
blocks 3/7 and runs 2/2 (its rejections are runtime NameErrors).
Naive exec blocks 0/7. Reproduce: cargo test -p lex-cli --test
agent_sandbox_bench.

A few other things that fell out of "the AST is the interface":
AST-native diff (renames register as renamed, not delete+add),
three-way structural merge with JSON conflicts instead of
<<<<<< markers, content-addressed stage store with `lex blame`.

Repo: https://github.com/alpibrusl/lex-lang
Landing page: https://alpibrusl.github.io/lex-lang/

Honest weak spot: capability ≠ correctness. A `[net]`-granted
body can still exfiltrate. Spec proofs (Z3 export) cover the
gap partially. Curious how others have thought about this.
```

### Notes

- /r/programming gets a lot of low-effort posts. Title should
  do work — "designed for code no one will read" is a hook,
  but you need substance in the first two paragraphs to
  survive the cynicism.
- Expect "this is just capabilities with extra steps" comments.
  Reply factually, not defensively.

---

## /r/MachineLearning (only if angle holds)

This sub trends toward research/experimental ML. Lex is not an
ML project; only post here if framed specifically around the
agent-tool sandbox use case, and expect it to land flat unless
you have measurable LLM safety numbers.

### Title (only post with substance)

```
[P] Sandboxing LLM-emitted tool code with type-system-enforced effects: 7/7 vs 3/7 RestrictedPython
```

### Notes

Don't post here unless you have a concrete LLM-safety-research
framing and are ready to defend the bench against "but adversarial
ML benchmarks are worth nothing." If unsure, skip.
