---
name: Bug report
about: Something is doing the wrong thing
labels: bug
---

**What did you expect?**

A clear, one-sentence description of the expected behavior.

**What happened instead?**

What actually happened. Paste the verbatim error if there is one.

**Reproducer**

The smallest `.lex` source / shell command that triggers the bug.

```lex
fn example() -> Int { 1 }
```

```bash
$ lex check example.lex
```

**Environment**

- `lex --version` output:
- OS / arch:
- Rust toolchain (`rustc --version`):

**Additional context**

Anything else that helps — recent changes, suspected component
(parser, type checker, runtime, store, CLI), etc.
