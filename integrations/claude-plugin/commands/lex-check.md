---
description: Type-check Lex source and, on failure, surface the structured errors (rule_tag + suggested_transform) ready for a repair step.
argument-hint: "[file-or-dir]"
allowed-tools: Bash(lex:*), Read
---

Type-check the Lex target: `$ARGUMENTS` (default to the project `src/` if no
argument is given).

1. Run `lex check --strict $ARGUMENTS`.
2. If it passes, report `ok` plus any required-effects hint and stop.
3. If it fails, re-run with `lex --output json check $ARGUMENTS` and, for each
   error, report:
   - `file:line:col`
   - the `rule_tag` and `rule_explanation`
   - the `suggested_transform` verbatim, if present
4. Do **not** fix anything by broadening an effect signature. If there is a
   `suggested_transform`, recommend `/lex:lex-repair` to apply it. If the fix
   is a body change, propose the narrowed body but wait for confirmation
   before editing.
