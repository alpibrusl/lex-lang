# Reddit

Reddit fragments by sub. Pick one or two — don't blast all four
the same day, the mod queues notice cross-posts and people downvote
them.

## /r/rust

Audience: Rust developers. Lead with the implementation, not the
language pitch.

### Title

```
Lex: a small functional language in Rust with effect-typed sandboxing for LLM-emitted tool bodies
```

### Body

```text
Hi r/rust — sharing a project I've been working on for a while.

Lex is a functional language whose type system encodes effects
in function signatures (`fn f(...) -> [net] Result[Str, Str]`).
The implementation is a Rust workspace: lex-syntax (logos lexer
+ a hand-written parser), lex-ast (canonical AST + structural
diff/merge), lex-types (the effect-aware type checker),
lex-bytecode (stack-based VM), lex-runtime (effect handlers —
gated tiny_http server, ureq client, tungstenite WS),
lex-store (content-addressed sig/stage store), lex-cli (the
`lex` binary), plus a Core sibling for sized numerics + tensor
shape arithmetic.

A few Rust-flavored choices that turned out useful, since this
is r/rust:

- **logos for the lexer.** ~70 tokens, derives the DFA. The
  parser ends up the bottleneck, not the lexer.
- **rustls + platform-verifier via ureq 3.x** for the HTTP
  client. Native cert store, no native-tls dependency.
- **tungstenite 0.29 for the WS chat builtin.** Per-connection
  worker thread + mpsc::channel for outbound; the room registry
  is `Arc<Mutex<IndexMap<...>>>`. Lex code stays pure, so the
  shared state lives in the host runtime, not the language.
- **ACLI-compliant CLI** (`lex --output json introspect`), so
  any LLM agent can drive it without a bespoke skill file. The
  `.cli/commands.json` is generated and committed so agents
  browsing the repo can read it without running the binary.

The part I'd most like feedback on: the effect-checker threads
the path of effect inheritance through `flow.sequential` /
`flow.branch` so that when a higher-order combinator's body
has an unexpected effect, the error points to the inner lambda,
not the combinator call site. Not novel — just a lot of careful
span bookkeeping — but the output is legible and I'd like to
hear if there's a cleaner way.

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
Lex: row-based effects in a small functional language for sandboxing LLM-emitted code
```

### Body

```text
I've been working on a small functional language with effects
in the function signature — not algebraic effects (no handlers
in user code), just a closed row of capability tags that
propagate through application and pattern-match.

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
   opaque are silent capability leaks. The cost is real:
   domain effects (db, email, …) have to ride on top of these
   plus host-level policy. Wrong call?

2. **No effect polymorphism in user syntax.** Functions are
   parameterized over types but not effect rows. The error
   messages are much better, but combinators like
   `flow.sequential` end up monomorphized per effect set in
   the stdlib. Fair tradeoff or papering over a missing
   feature?

3. **Effects as part of the canonical AST hash.** Two functions
   with the same body but different effect annotations get
   different StageIds in the content-addressed store. This is
   the basis for `lex blame` — per-fn lifecycle history that
   treats an effect change as a new stage, not an edit. I'm
   less sure about this one than the other two.

The motivating workflow is sandboxing LLM-emitted tool bodies:
the host declares `--allow-effects net` and any body that
reaches outside is rejected at type-check, before execution.
Adversarial bench (7 attacks + 2 benign) at bench/REPORT.md
compares against RestrictedPython; the gap is mostly *when*
the rejection happens, not cleverness.

Spec sibling adds randomized property checking + SMT-LIB 2
export to Z3 for behavioral contracts. Capability ≠ correctness;
the type system answers "what does this code touch", not
"is the touch wise."

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
A small language for sandboxing the code AI agents write
```

### Body

```text
I run agent-generated code locally as part of my workflow,
and I kept wanting a sandbox where the host could say "this
body can touch the network and nothing else" and have the
type checker actually enforce it — not catch it as a runtime
exception after something already escaped. Lex is what fell
out of that.

Effects are part of the type:

    fn fetch(url :: Str) -> [net] Result[Str, Str]

If the body of fetch tries to read a file, the type checker
rejects the whole program before any byte runs. The runtime
also re-checks the policy at the dispatch site, so a function
declared with `[fs_read("/data")]` and granted at startup
still has to pass the path check at the actual read.

To check whether the idea held, I ran an adversarial bench:
7 attacks + 2 benign cases against naive Python exec,
RestrictedPython, and Lex. Lex blocks 7/7 and runs 2/2;
RestrictedPython blocks 3/7 and runs 2/2; naive exec blocks
0/7. The honest read is that most of the gap is *where* the
rejection happens, not how clever the rules are: Lex rejects
at type-check, RestrictedPython at runtime. I picked the
attacks myself, so take the numbers with that grain of salt.
Reproduce: `cargo test -p lex-cli --test agent_sandbox_bench`.

A few other things that fell out of "the AST is the
interface": AST-native diff (renames register as renamed,
not delete+add), three-way structural merge with JSON
conflicts instead of <<<<<< markers, content-addressed
stage store with `lex blame`.

Honest weak spot: capability ≠ correctness. A `[net]`-granted
body can still exfiltrate; effect typing answers what the
code touches, not whether the touch is wise. Spec proofs
(Z3 export) cover that gap partially. Curious how others
have thought about it.

Repo: https://github.com/alpibrusl/lex-lang
Landing page: https://alpibrusl.github.io/lex-lang/
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
