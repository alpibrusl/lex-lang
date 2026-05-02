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
Lex: effect types in the function signature, checked before the body runs
```

## URL

Submit the repo, not the landing page:
`https://github.com/alpibrusl/lex-lang`

## Author comment (post immediately)

```text
I built Lex because I kept watching agents emit tool bodies and
wanted the host to be able to say "this can touch the network and
nothing else" in a way the type checker enforced. Effects are part
of the type:

    fn fetch(url :: Str) -> [net] Result[Str, Str]

A body that calls io.read in there is rejected at type-check —
no runtime NameError, no exec-time exception. The runtime then
re-checks the policy at the dispatch site (per-path / per-host),
so static + dynamic both fire.

The hairier design questions, since this is Lobsters:

1. Effects are a closed grammar (io, net, fs_read, fs_write, time,
   rand, proc, budget, chat). Adding one is a language change. I
   went with closed because every host has to learn what to enforce;
   a user-defined effect that the runtime doesn't know about is a
   silent capability leak. Open to being wrong about this.

2. Effect typing is row-based with subtyping; if `f` declares
   `[net]` and `g` declares `[net, io]`, you can pass `f` where
   `g` is expected but not vice-versa. There's no effect
   polymorphism (no row variables in user code). That keeps the
   error messages legible at the cost of some `flow.sequential`
   verbosity.

3. The bench at `bench/REPORT.md` compares Lex against naive
   exec and RestrictedPython on 7 attacks + 2 benign cases. Lex
   is 7/7 + 2/2 because the rejection happens at type-check;
   RestrictedPython is 3/7 + 2/2 because its blocks are runtime
   NameErrors after AST rewrite. The methodological caveat is
   that I picked the attacks — happy to take suggestions for
   ones that should embarrass Lex.

Capability ≠ correctness. A `[net]`-granted body can still
exfiltrate. Spec proofs (`lex spec check`, SMT-LIB 2 export to
Z3) cover the gap partially.

Repo includes a sandboxed `lex agent-tool` binary that wires
this to Anthropic's API, a content-addressed stage store with
`lex blame` for per-function history, and AST-native diff/
three-way merge as fallout from "the AST is the interface."

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
