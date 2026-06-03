# lex-sprint

> **Incubator.** Lives in `lex-lang/lex-sprint/` while the design and API
> surface firm up — the same path `lex-jobs` took. Intended to extract to
> a standalone `lex-sprint` package once the design has seen real use.
> (The standalone repo could not be created from the authoring session;
> this doc is self-contained and transplants unchanged.)

Multi-agent **sprint cycles** for the Lex ecosystem — take a project
request and drive it end-to-end (Intake → Design → Implementation → QA →
Demo → Retro → Digest), with a **dynamic** agent graph derived per
request and refined after design.

> **Status: design.** No implementation yet. The architecture lives in
> [`docs/design/sprint-cycles.md`](docs/design/sprint-cycles.md) — read
> it first. The first code PR implements M1 (graph + phase + meta-spec).

## The idea in one line

> *"the substrate carries the constraints; the model fills the bodies;
> the type system verifies the result."* — [Trust Without Comprehension](https://alpibru.com/manifesto), §VI

The **interaction graph is data** (a typed, content-addressed
`SprintGraph` produced by an Architect agent), and the **executor is a
fixed, effect-typed interpreter** of that graph. No agent trusts another
by reading its output — it trusts a typed handoff, a spec gate evaluated
at both ends, an honest effect row, and a hash-chained attestation.

## Built on

lex-llm · lex-agent (A2A) · lex-spec · lex-schema · lex-trail · lex-jobs ·
lex-hub · lex-vcs · lex-mcp · lex-code. See the design doc's reuse map —
lex-sprint is mostly wiring on top of these.

---

Built under the principles of [Trust Without Comprehension](https://alpibru.com/manifesto).
