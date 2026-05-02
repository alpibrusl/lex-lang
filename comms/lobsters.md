# Lobsters

Lobsters is technical, skeptical, and dislikes anything that smells
like a press release. Lead with code, not motivation. Tags help —
pick narrowly.

## Tags

`programming`, `rust`, `plt`, `ai`

(Drop `ai` if Lobsters' anti-AI mood is hot that week; the language
stands on PLT + sandboxing without it.)

## Title

```
Lex: a small functional language with effects in the function signature
```

## URL

Submit the repo, not the landing page:
`https://github.com/alpibrusl/lex-lang`

## Author comment (post immediately)

```text
I built Lex because I kept running agent-emitted tool bodies
locally and wanted the host to be able to say "this can touch
the network and nothing else" in a way the type checker
enforced. Effects are part of the type:

    fn fetch(url :: Str) -> [net] Result[Str, Str]

A body that calls io.read in there is rejected at type-check —
no runtime NameError, no exec-time exception. The runtime
re-checks the policy at the dispatch site (per-path / per-host),
so static + dynamic both fire.

Three design choices I'd actually like to argue about, since
this is Lobsters:

1. Effects are a closed grammar (io, net, fs_read, fs_write,
   time, rand, proc, budget, chat). Adding one is a language
   change. I went with closed because the runtime has to know
   what to enforce — a user-defined effect the runtime treats
   as opaque is a silent capability leak. The cost is real
   though: domain effects (db, email, …) ride on top of these
   plus host-level policy rather than living in the type
   system. Genuinely open to being wrong about this.

2. Effect typing is row-based with subtyping. `f : [net]` is
   passable where `g : [net, io]` is expected, not vice-versa.
   No effect polymorphism in user syntax (the internal
   representation has row variables; the surface doesn't).
   That keeps the error messages legible at the cost of some
   `flow.sequential` verbosity. Right tradeoff?

3. The bench at `bench/REPORT.md` compares Lex against naive
   exec and RestrictedPython on 7 attacks + 2 benign cases:
   7/7 vs 3/7 vs 0/7 blocked. Most of that gap is *where*
   the rejection happens, not how clever the rules are. I
   picked the attacks myself, so the numbers come with that
   grain of salt — happy to take suggestions for cases that
   should embarrass Lex.

Capability ≠ correctness, and I want to be upfront. A
`[net]`-granted body can still exfiltrate. Spec proofs
(`lex spec check`, SMT-LIB 2 export to Z3) cover that gap
partially; the rest I don't have a great answer for.

Repo includes a sandboxed `lex agent-tool` binary wiring
this to Anthropic's API, a content-addressed stage store
with `lex blame` for per-function history, and AST-native
diff / three-way merge as fallout from "the AST is the
interface."

285 tests, EUPL-1.2, ~14 Rust crates.
```

## Posting notes

- Lobsters is invite-only — use the account that's actually
  active. Don't burn an invite to seed-post.
- Don't cross-post to HN within 48h. Lobsters notices.
- If the post lands flat, **don't bump it**. Move on.
- Expect at least one comment about "why not Koka / Eff /
  algebraic effects" — link to `docs/index.html`'s "Why this
  differs" section if it covers the angle, otherwise reply
  honestly that Lex is a deliberately small subset.
