---
name: Feature request
about: Propose an addition or change
labels: enhancement
---

**The problem**

What can't you do today? One paragraph.

**Proposed solution**

Concrete API, CLI flag, syntax, or design sketch. The more specific,
the easier to discuss.

**Alternatives considered**

What else have you tried or thought about? Why is this proposal
better than the alternatives?

**Affected components**

Which crates / commands does this touch?
- [ ] `lex-syntax` (parser)
- [ ] `lex-ast` (canonical AST)
- [ ] `lex-types` (type / effect system)
- [ ] `lex-bytecode` (compiler / VM)
- [ ] `lex-runtime` (effects, stdlib)
- [ ] `lex-store` (content-addressed store, branches)
- [ ] `lex-cli` (CLI surface)
- [ ] `lex-api` (HTTP server)
- [ ] Spec / docs only

**Backwards-compatibility impact**

Does this change `SigId` / `StageId` hashes, the JSON wire format,
the canonical AST, or any documented CLI flag? Pre-1.0 we'll ship
breaking changes when justified, but we want to know up front.
