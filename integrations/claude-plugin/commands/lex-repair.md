---
description: Run the repair-not-regenerate loop — turn a lex check failure into a typed lex repair --apply, landing a RepairAttempt attestation instead of rewriting the body.
argument-hint: "[file] [op_id?]"
allowed-tools: Bash(lex:*), Read
---

Repair the failing Lex op rather than regenerating it. Target: `$ARGUMENTS`.

Follow the rule from `lex agent-guidelines` §4:

1. Get the structured error: `lex --output json check $1`. Read the
   `rule_tag` and `suggested_transform` from the first error.
2. Identify the failing `op_id` (use the second argument if provided, else
   find it via `lex blame <fn> --with-evidence` or the publish output).
3. Apply the suggested transform **verbatim**:
   ```bash
   lex --output json repair <op_id> \
     --apply --transform '<suggested_transform>' --store .lex-store
   ```
   Expect `{"outcome":"passed","applied_op_id":"op_..."}`. The attempt lands
   as a `RepairAttempt` attestation linked to the originating hint.
4. Re-run `lex check --strict $1` to confirm.
5. If repair fails, inspect the new error and try **once** more. After **two**
   failed repair attempts, stop and report that the body's design — not its
   syntax — is wrong; recommend an intentional regeneration that keeps the
   signature (and thus the SigId + attestations) stable.

Never broaden an effect signature to make the error disappear.
