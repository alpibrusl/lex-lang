# LinkedIn

LinkedIn rewards story + numbers, punishes hacker-skepticism.
Different audience: founders, engineering managers, security
leadership, and the long tail of dev-tool buyers. Use it when
you want reach beyond the HN/Lobsters circle, not when you
want a technical critique.

## Length guidance

- 1300-char limit on the "see more" fold; everything past is hidden
  by default. Put the hook + headline number above the fold.
- Around 1500–2500 chars total for the post body works well.
- Native carousels and embedded video out-perform external links;
  consider uploading the asciinema GIF directly rather than just
  linking.

## Post

```text
Most security tooling assumes humans review the code that runs.

Agents are about to break that assumption — and once they do,
the function signature has to become the contract.

I've been building Lex, a small functional language whose type
system encodes effects in the signature itself:

   fn fetch(url) -> [net] Result[Str, Str]

If the body tries to touch the filesystem, the program is
rejected at type-check, before any byte runs.

To test the idea, I ran 7 adversarial attacks + 2 benign cases
through three sandboxes:

  • Naive Python exec:        0 / 7 attacks blocked
  • RestrictedPython:         3 / 7 attacks blocked
  • Lex:                      7 / 7 attacks blocked

The difference isn't cleverer rules. It's where the rejection
happens:

  • RestrictedPython rejects at runtime — a NameError after the
    AST has been rewritten and execution has started.
  • Lex rejects at type-check — pre-execution. The body never
    runs.

The motivating workflow is `lex agent-tool`: ask Claude or
Codex for a tool body, splice it into a fixed signature, and
run it under a declared effect set. Anything outside the set
fails to compile.

I'd love feedback from anyone running LLM-generated code in
production — what's your current sandboxing layer, and where
does it leak?

Repo (open source, EUPL-1.2):
https://github.com/alpibrusl/lex-lang

Bench methodology + per-case breakdown:
https://github.com/alpibrusl/lex-lang/blob/main/bench/REPORT.md
```

## Posting notes

- **Time:** Tuesday or Wednesday, 8–10am in your audience's
  timezone. LinkedIn engagement decays fast outside business
  hours.
- **No hashtags in the body.** Add 3–5 at the bottom if you
  want discovery: #softwareengineering #typesafety
  #aisecurity #programminglanguages #rust
- **Drop the GIF inline.** Either the asciinema cast (LinkedIn
  supports MP4 upload) or a static screenshot of the type-
  check rejection. Inline media beats external links.
- **Reply to every comment** in the first 4 hours. LinkedIn's
  feed algorithm rewards thread depth.
- **Tag people sparingly.** One or two collaborators or
  reviewers who'd genuinely want to see this. Mass-tagging
  gets the post throttled.
- **Don't repost the HN copy verbatim.** Different audience,
  different register. The HN draft asks "is the closed effect
  grammar the right call" — that question won't resonate
  here. Save it for HN.
