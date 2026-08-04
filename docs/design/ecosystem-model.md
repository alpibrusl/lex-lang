# The Lex product ecosystem — loom, ctl, soft, os

Status as of 2026-08-03. Clarifying doc, no code changes here. Written to give
downstream repos one place to point at instead of each restating (and each
independently drifting on) the same cross-repo positioning claims.

## The model

Three product repos sit on top of `lex-lang`, on two independent axes:

1. **A company** ([lex-loom](https://github.com/alpibrusl/lex-loom)) — one
   organization's own build → distribute → operate → strategize loop, run as
   a persistent goal iterating a series of sprints.
2. **Interactions between companies**
   ([lex-soft](https://github.com/alpibrusl/lex-soft)) — the cross-org
   mechanism layer: identity, agent mesh, trust, and evidence-gated
   settlement, with vertical logic living in separate domain packs.
3. **An optional sandboxed runtime**
   ([lex-os](https://github.com/alpibrusl/lex-os)) — a sealed, disposable
   box plus a goal, supervised by something the agent can't reach. Neither
   (1) nor (2) requires it; it's a place either can optionally *run*.

(1) and (2) are peers on the same axis — a company, and the mesh of
companies talking to each other. (3) is orthogonal: a hosting concern for
either's agent workloads, not a third thing of the same kind.

```
                        lex-lang (language, effect types, attestation graph)
                                        │
        ┌───────────────────────────────┼───────────────────────────────┐
        │                                │                                │
    lex-loom                        lex-ctl                          lex-soft
  (a company: build→               (shared control-              (interactions between
   distribute→operate→              plane kernel — contracts,      companies: identity,
   strategize, one org's            verify, tier, damping —        mesh, trust, evidence-
   own loop)                        "no action without a           gated settlement)
        │                            checkable predicted            │
        │   Operate loop v1          effect")                       │  forward-looking
        └──────── consumes ─────────────▲──────── consumes ─────────┘  domain-pack verdicts
                                         │                              (lex-soft#106)
                                         │
                              (planned: lex-trail for event
                               recording, lex-agent for
                               capability gates)

                    ┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄┄
                       orthogonal axis — optional, opt-in
                                         │
                                     lex-os
                        (sealed disposable box + capability
                       grant; can host either's agent workloads)
```

## lex-ctl is a shared kernel, not a loom feature

It's tempting to read `lex-ctl` as something loom built for itself that soft
borrows. That's not what its own README says: it ships **no action
vocabulary, no thresholds, no baselines, no scheduler** — hosts supply all
of that. The kernel is the domain-agnostic mechanism: an `EffectContract`
(signal, typed predicate, deadline, confidence, falsification behavior), a
pure `verify` judgment (`Materialised / Falsified / Ambiguous`), and
`tier`/`stability` — autonomy tiers *earned* from measured hit rate, plus
dwell locks, hysteresis, and a circuit breaker so an agent can't oscillate
the system it acts on.

It is explicitly **not** a dashboard: *"a dashboard is a cached, lossy
projection of a database, chosen in advance by someone who won't be present
at read time... the human read path is a verification surface, not a query
surface."* The loop it encodes — sensing → incident → capability gate →
typed actuation → scheduled verifier → ledger — replaces what a
dashboard-plus-human-judgment workflow would do, structurally: autonomy is
granted by track record, not by static trust or a human glancing at a chart.

Two consumers exist today, deliberately different domains, to prove the
kernel's API stays domain-agnostic:

- **lex-loom** — the Operate loop v1 controller (`lex-loom#118`): sensing
  over `company_operate_signals` (liveness, error rate, usage, cost),
  incident diagnosis, capability-gated actuation, effect verification, and
  an auto-tier decision layer behind a circuit breaker.
- **lex-soft** — forward-looking verdicts for domain-pack actions
  (`lex-soft#106`): `src/ctl.lex` mounts the kernel
  (`mount_ctl(router, db)`), exposing `POST/GET /ctl/contracts`; a
  `judge_and_record` pass calls `verify.judge` against a host-supplied
  `observe` closure, records to the trail, and notifies via the outbox. The
  worked example (`examples/ctl-sketch/charge_remediation.lex`) restarts a
  stalled EV charging session — deliberately unlike loom's "restart a
  server" case.

Two more consumers are named but not yet built (`lex-ctl` README, "Planned
integrations"): `lex-trail` (contracts/dispositions as recorded events) and
`lex-agent` (tier decisions feeding capability gates). Both would make this
infrastructure ecosystem-wide rather than a two-repo arrangement.

## lex-os: wired for neither, designed for one

| Integration | Status |
|---|---|
| loom ↔ lex-ctl | **Wired.** Operate loop v1, see above. |
| soft ↔ lex-ctl | **Wired.** `src/ctl.lex`, see above. |
| loom ↔ lex-os | **Phase 0 wiring started (2026-08-03).** `lex-os` gained a new `exec` primitive (mediate one external command through a manifest's grant, then run it *inside a booted box* — smaller and more honest than either `run`'s agent loop or `capsule install`'s Lex-program entrypoint, and the right fit for a caller like loom's proc executor). It boots `lex-os-guest` in a one-shot mode and supports the real Firecracker backend the same way `lex-os run` does, not just a simulated policy check. loom's `src/agent/runner.lex` routes `proc_cmd` nodes through it when `LEX_OS_ISOLATION` is set (opt-in, off by default), gated by a per-role `Grant` from `src/manifests.lex` (`manifest_json_for_kind`) — but loom's own call still hardcodes `--simulated`. Verified wire-compatible with lex-os's real manifest parser and the QA-denied/Build-allowed behavior the design doc called for, including a real spawned-and-captured child process through the full mediation protocol. Still open: dropping loom's hardcoded `--simulated` (needs a KVM CI runner to validate against), the LLM and A2A executors (unmediated), and a live end-to-end test in loom's own CI (which doesn't install the `lex-os` binary yet). See `docs/design/lex-os-isolation.md` in lex-loom. |
| soft ↔ lex-os | **Not started.** No dependency declared. Its tools are already effect-scoped narrowly (`[net, io, proc]`, no direct `sql`), which is compatible with the grant model if this is ever built. |
| loom ↔ soft (direct) | **No runtime dependency.** loom vendored its own agent runtime specifically so it has none. Historical lineage is documented (loom's `docs/design/sprint-cycles.md §17`); the only *live* connection today is the shared `lex-ctl` kernel, not a direct API between the two. |

Also worth noting: `lex-os`'s own README is accurate about its own limits —
it already states that `capsule install --run` provisions under the
*simulated* perimeter only (not a security boundary), tracked as
`lex-os#36`. That gap is real and orthogonal to this doc; it isn't specific
to hosting loom or soft.

## Open questions

- Should loom and soft ever integrate directly (e.g. a company's agents
  appearing as A2A peers inside soft's mesh), or does the shared `lex-ctl`
  kernel cover the integration that's actually needed between them?
- If sandboxing under `lex-os` becomes a real priority, loom is the natural
  pilot — it already has grant generation; soft has no manifest work
  started yet.
- Track `lex-ctl`'s two planned consumers (`lex-trail`, `lex-agent`) as they
  land — they extend this from a two-repo arrangement to shared
  infrastructure.

## Maintenance rule

Positioning claims — what's wired vs. designed-but-not-wired vs. not started
— belong **here**, updated in one place. Each product README should carry
only a short pointer back to this doc, not a restatement of the same
claims; three independent copies of the same status table go stale
independently, which is exactly what happened to `lex-loom`'s README before
this doc existed.
