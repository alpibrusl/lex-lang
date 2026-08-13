# Lex vs. "Code as Agent Harness" — positioning and gap analysis

**Status:** clarifying doc, no code changes here. Maps the survey
*Code as Agent Harness* (Ning, Tieu, Fu, et al., arXiv:2605.18747,
May 2026) onto the Lex feature set, and records the gaps worth
turning into tracked issues. Companion paper index:
<https://github.com/YennNing/Awesome-Code-as-Agent-Harness-Papers>.

## Why this doc exists

The survey's thesis is that code is no longer only an agent's
*output* — it is the operational substrate agents run on: the medium
for reasoning, action, environment modeling, and execution-based
verification. It organizes the field into three layers:

1. **Harness interface** — how code connects agents to reasoning,
   action, and environment modeling.
2. **Harness mechanisms** — planning, memory, and tool use for
   long-horizon execution, plus feedback-driven control and
   optimization.
3. **Multi-agent scaling** — shared code artifacts supporting
   coordination, review, and verification across agents.

Lex is a language-level instance of that thesis ("the contract layer
agents emit into"). The survey gives us (a) a vocabulary reviewers
and users already recognize, and (b) a checklist to find the
mechanisms we haven't built yet. This doc does both. It changes no
contracts; everything here is either positioning or a proposal
pointer.

## Layer-by-layer mapping

### Layer 1 — Harness interface

| Survey concern | Lex today | Status |
|---|---|---|
| Action interface | `lex agent-tool`, `lex serve` (HTTP + MCP), ACLI (`lex skill`, `--output json`, stable exit codes) | ✅ shipped |
| Environment modeling | Effect rows (`[net]`, `[fs_write("/tmp/…")]`, `[llm_cloud]`) as a machine-checked model of what a body may touch | ✅ shipped |
| Pre-execution safety | Type-checker rejection before a byte runs; 7/7 adversarial cases in [`bench/REPORT.md`](../../bench/REPORT.md) | ✅ shipped |
| Environment *simulation* | `lex-trace` records runs ([trace-vs-vcs](trace-vs-vcs.md)), but there is no replay-as-verification mode | ⚠️ gap G3 |

### Layer 2 — Harness mechanisms

| Survey concern | Lex today | Status |
|---|---|---|
| Feedback-driven control | `RepairHint` → `suggested_transform` → `lex repair --apply`; each attempt lands as a `RepairAttempt` attestation | ✅ shipped |
| Execution-based verification | `examples {}` blocks run at `lex check` time; `Spec` / `DiffBody` / `SandboxRun` attestations | ✅ shipped |
| Feedback-driven optimization | `ProducerTrust` over a rolling attestation window; per-session budget gate | ✅ shipped |
| Planning | `Intent` records `(prompt, model, session)` and groups the ops it produced — prompt-level provenance, not a structured plan | ⚠️ gap G1 |
| Memory | Append-only op log + attestation graph are a *record*; query surface is `lex blame` / `lex log`, not a general recall interface | ⚠️ gap G2 |

### Layer 3 — Multi-agent scaling

| Survey concern | Lex today | Status |
|---|---|---|
| Shared code artifacts | Content-addressed AST (SigId / StageId / OpId), append-only op log | ✅ shipped |
| Coordination | `Candidate / Promote` proposer races without CAS contention; structural merge with typed JSON conflicts | ✅ shipped |
| Verification across agents | `lex blame --with-evidence` walks the attestation chain; `ProducerTrust` scores producers | ✅ shipped |
| Review | Human verdicts exist as attestations (`Override`, `Defer`, `Block` / `Unblock`), but there is no structured **agent-issued** review artifact between `Candidate` and `Promote` | ⚠️ gap G4 |

## Gaps

None of these are commitments; each is a candidate issue with the
survey layer it closes. Ordered by (my guess at) leverage per effort.

### G4 — `Review` attestation kind (layer 3)

Today the path from `Candidate` to `Promote` carries no structured
verdict: a promoting agent either promotes or doesn't. The
`AttestationKind` enum already has the right shape for the fix —
human verdicts (`Override { actor, reason, .. }`) are attestations,
so an agent-issued `Review { actor, verdict, findings }` kind slots
in beside them. That gives `lex blame --with-evidence` review
provenance for free and lets `ProducerTrust` weight *reviewers*, not
just producers. Smallest of the four gaps; touches `lex-vcs`
(`attestation.rs`) plus a CLI verb.

### G1 — structured plans on top of `Intent` (layer 2)

`Intent` already gives us content-addressed, prompt-level
provenance: same `(prompt, model, session)` → same `IntentId`, and
every op carries an optional `intent_id`. What it does not capture
is *decomposition* — an agent's multi-step plan with ordering or
dependency edges between steps. A `Plan` artifact (own namespace,
like `<root>/intents/`) whose steps reference the intents/ops that
discharged them would let `lex blame` answer "what plan step
produced this change" and would give long-horizon agents a durable,
diff-able plan representation instead of scratchpad text. Design
constraint: like `intent_id`, plan references must stay **out of
OpId canonical form** (see [`INVARIANTS.md`](../INVARIANTS.md)) so
attaching a plan never perturbs hashes.

### G2 — attestation recall surface (layer 2)

The attestation graph is agent memory in the survey's sense, but the
only ergonomic reads are `lex blame <fn>` (per-function) and
`lex log` (linear). A query verb — e.g.
`lex recall --kind spec --since <op> --producer <id>`, JSON out —
would let an agent rehydrate "what do we already know about this
region of the store" without walking the log itself. Pure read-side
addition; no new persisted state.

### G3 — trace replay as verification (layer 1)

`lex-trace` already persists per-run `TraceTree`s outside the op log
([trace-vs-vcs](trace-vs-vcs.md)), and the audit-replay recipe there
covers cross-store sync. The missing piece is a *verification mode*:
run a modified body against a recorded trace's inputs with effects
stubbed from the trace, and emit the result as an attestation
(a `TraceReplay` kind next to `DiffBody`, which it generalizes —
`DiffBody` compares against a second body, `TraceReplay` compares
against a recorded run). This extends pre-execution *rejection* into
pre-execution *simulation*: "would the new body have behaved the
same on last week's production run" becomes a queryable fact, with
no effect grants issued.

### Non-gap worth naming — verifiable rewards

`examples {}` outcomes + the attestation log are already a clean
reward signal for training or benchmarking agent policies (the
survey's execution-based-verification thread points straight at
this). Nothing to build in-tree; noting it so downstream RL /
evaluation projects know the signal exists and where it lives.

## Positioning to-dos (docs only)

- Cite the survey where we explain what Lex *is* (README "How it
  works" intro, `docs/AGENT.md`): one sentence + arXiv link. Lex
  predates neither the ideas nor the field; the win is shared
  vocabulary, not priority claims.
- [`bench/REPORT.md`](../../bench/REPORT.md) can adopt the survey's
  terms ("execution-based verification", "harness interface") when
  describing the 7/7 sandbox result — makes the report legible to
  people arriving from the paper.
- The survey's companion repo curates harness systems; Lex belongs
  in it (see outreach note below — that's an external PR, not a
  change here).

## Non-goals

- No new op kinds, attestation kinds, or CLI verbs land with this
  doc. G1–G4 each need their own issue + design review; G1 and G3
  interact with hash-stability contracts and must clear
  [`INVARIANTS.md`](../INVARIANTS.md) /
  [`canonicalization.md`](canonicalization.md) first.
- Not a survey summary. Read the paper; this doc only records where
  Lex stands relative to its taxonomy.
