# `agent_merge` — agent-native VCS, end-to-end

A scripted walkthrough of `lex-vcs`'s tier-2 features (#128). Two
"agents" work in parallel on the same function, produce a
ModifyModify conflict, and a third agent (this script) resolves
it programmatically — no text editing, no merge markers, no
re-running `lex check` to make sure the result still typechecks
because the store-write gate (#130) does that on every accepted
op automatically.

What this example demonstrates, mapped to the issues:

| Concept | Issue | Surface used |
|---|---|---|
| Operation as the unit of change | #129 | every `publish` produces a typed op |
| Write-time type-check gate | #130 | `publish_program` rejects bad source pre-commit |
| TypeCheck attestation auto-emitted | #132 | `lex stage <id> --attestations` |
| Stateful merge session | #134 | `lex merge start \| resolve \| commit` |
| Spec attestation as evidence | #132 | `lex spec check --store DIR` |
| Evidence trail per stage | #132 | `lex blame --with-evidence`, `lex attest filter` |

## The scenario

Two agents working in parallel modify `clamp(x, lo, hi) -> Int`:

- `feature` (one agent): refactors to `min2(max2(x, lo), hi)`.
- `main` (a different agent, or the human): adds an early-return
  guard for the degenerate `lo > hi` case.

Both bodies typecheck. Both pass the same spec
(`r >= lo and r <= hi` when `lo <= hi`). But the AST diverged on
both sides, so a merge sees a `ModifyModify` conflict on the
`clamp` sig.

In a Git world the agent would now be reading text merge
markers and guessing whether a hunk is right. Here, the agent
gets the structured conflict over JSON and submits a typed
resolution.

## Run it

The whole flow is one script. It uses an ephemeral store under
`$STORE` (default: a fresh `mktemp` dir) so it can't disturb a
real `~/.lex/store`.

```sh
cd <repo-root>
bash examples/agent_merge/run.sh
```

You should see eight stages of output (numbered 1–8) walking
the merge from publish through to the evidence trail. The most
interesting are:

- **Step 1b** — the `TypeCheck::Passed` attestation that
  `Store::publish_program` wrote *automatically* when the v0
  body landed. No `lex check` was run; the gate ran inside the
  store-write path.
- **Step 4** — the merge surfaces the conflict as JSON, with
  `ours` / `theirs` / `base` stage_ids the agent can compare
  programmatically. Compare to `git diff --merge` and the line
  noise around `<<<<<<<` markers.
- **Step 6** — the merge op lands as a single typed operation
  with the right parents in the DAG. `lex log main` shows it
  alongside the original publishes.
- **Step 8** — `lex blame --with-evidence` and `lex attest
  filter` read back the cumulative evidence: every TypeCheck
  the gate emitted plus the Spec we just persisted.

## Adapt this for an agent harness

The shell script substitutes a hard-coded `take_theirs` for the
agent's decision in step 5. A real harness would:

1. Read `merge start`'s JSON conflict list.
2. For each conflict, fetch the `ours` / `theirs` stages via
   `GET /v1/stage/<id>` (or `lex stage <id>`).
3. Pick a resolution per conflict (`take_ours` / `take_theirs` /
   `custom` with a brand-new op / `defer` to a human).
4. POST the batch to `/v1/merge/<merge_id>/resolve` (or `lex
   merge resolve`).
5. POST `/v1/merge/<merge_id>/commit`.

The HTTP and CLI surfaces are 1:1 — same JSON shape, same
verdicts. Pick whichever fits the harness; `lex serve` keeps
the store hot if the agent runs many merges per session.

## Predicate branches (#133)

This example uses snapshot branches (`lex branch create
feature`). The same VCS supports predicate-defined branches —
saved queries over the op log:

```sh
# "All ops produced under intent ses_3781fc"
lex branch create my-view --predicate \
  '{"predicate":"intent","intent_id":"ses_3781fc"}'

# "Everything in main since the fork from feature"
lex branch peek feature --since-fork --vs main
```

That model is what makes 20 parallel exploration branches per
agent-task cheap (each is a small JSON predicate file, not a
copy of the world). See #133 for the full design.
