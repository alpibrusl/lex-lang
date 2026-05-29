---
description: Format, type-check, then publish Lex functions as typed Operations into the content-addressed store, reporting each fn's SigId / StageId / status.
argument-hint: "[file] [--activate]"
allowed-tools: Bash(lex:*), Read
---

Publish Lex source into the store. Target: `$ARGUMENTS`.

Gate before publishing so you don't churn SigIds or store a broken stage:

1. `lex fmt --check $1` — if it isn't canonical, run `lex fmt $1` (do **not**
   hand-format) and continue.
2. `lex check --strict $1` — must pass. If it fails, stop and hand off to
   `/lex:lex-repair`; do not publish a failing stage.
3. Publish:
   ```bash
   lex publish $ARGUMENTS        # add --activate to make the stages the head
   ```
   Report each function's `{name, sig_id, stage_id, status}`.
4. Confirm with `lex blame <fn> --with-evidence` that the expected
   TypeCheck / Examples attestations landed.

If you only want to preview, append `--dry-run` (exit code 9 + a
planned-actions envelope) and report the plan without writing.
