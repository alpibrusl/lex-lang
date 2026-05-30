---
description: Type-check then execute one Lex function under an explicit, narrow capability policy (args parsed as JSON).
argument-hint: "[file] [fn] [json-args...]"
allowed-tools: Bash(lex:*), Read
---

Run a Lex function. Invocation: `$ARGUMENTS` (file, then fn name, then any
JSON-encoded positional args).

1. Always type-check first: `lex check $1`. If it reports required effects,
   note them — you'll need matching grants.
2. Run under the **narrowest** policy that satisfies the declared effects.
   Pure functions need no grants:
   ```bash
   lex run $ARGUMENTS
   ```
   Effectful functions need explicit, scoped grants — never a blanket grant:
   ```bash
   lex run --allow-effects net --allow-net-host api.example.com $1 $2 <args>
   lex run --allow-fs-read /etc/myapp/ $1 $2 <args>
   ```
3. Report the output. If the run returns HTTP `503` with `Retry-After: 0`
   (budget exceeded), do **not** retry as-is — report it and recommend
   raising the cap or refactoring to spend less.
4. To capture a trace for later inspection, add `--trace`, then read it with
   `lex trace <id>`.
