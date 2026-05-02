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
Sharing something I've been working on for a while.

I run agent-generated code locally as part of my workflow,
and I kept wanting a sandbox where the host could say "this
body can touch the network and nothing else" and have a type
checker enforce it — not catch it as a runtime exception
after something already escaped.

Lex is the small language that fell out of that. A function
annotated

   fn fetch(url) -> [net] Result[Str, Str]

cannot reach the filesystem. If the body tries to read a
file, the program is rejected at type-check, before it runs.

To check whether the idea actually held, I ran a small
adversarial bench: 7 attacks + 2 benign cases through three
sandboxes. Numbers below — methodology and per-case detail
in the repo's bench/REPORT.md:

  • Naive Python exec:    0 / 7 attacks blocked
  • RestrictedPython:     3 / 7 attacks blocked
  • Lex:                  7 / 7 attacks blocked

The honest read is that most of the gap isn't cleverer rules.
It's where the rejection happens: RestrictedPython rejects at
runtime after the body has started; Lex rejects at type-check
before it runs. Different layer, not necessarily a better
idea — and I picked the attacks myself, so the numbers come
with that grain of salt.

Two things I want to be upfront about:

Capability ≠ correctness. A function granted [net] can still
exfiltrate data; the type system answers what your code
touches, not whether the touch is wise. Spec proofs cover
some of that gap. The rest is unclaimed.

A new language is a real ask. The pitch isn't "rewrite your
stack" — it's "the AI-emitted tool body lives in a 30-line
Lex fragment under a known effect set, and the rest of your
code keeps doing what it's doing." If that framing doesn't
help, this probably isn't for you.

If you run LLM-generated code in production, I'd genuinely
like to hear what your current sandboxing layer is and
where it leaks. Trying to learn what the gap actually looks
like in practice, not just in benchmarks.

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
