# The Lex product ecosystem — loom, ctl, soft, os

Status as of 2026-08-17. Clarifying doc, no code changes here. Written to give
downstream repos one place to point at instead of each restating (and each
independently drifting on) the same cross-repo positioning claims.
(Since the 2026-08-03 original: the soft/os-awareness epic `lex-loom#177`
landed — SA1–SA4, OA1–OA3 — so several rows below flipped from "designed"
to "wired". Superseded claims are replaced in place, per the maintenance
rule; git history is the archive.)

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
        │                                │                            │  (lex-soft#106)
        └────── loom consumes soft directly (SA1–SA4): ──────────────┘
           mesh registration + evidence-gated revenue settlement
                                         │
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
| loom ↔ lex-os | **Wired through OA3 (2026-08-17); real-KVM validation open.** `lex-os exec` mediates one external command through a manifest's grant and executes it *inside the booted guest* (real in-VM script mode over vsock, on both the simulated and Firecracker backends; per-backend denial dialects + runner-failure evidence, `lex-os#61`). On loom's side the epic `lex-loom#177` landed the whole enforcement story: **OA1** (`lex-loom#182`) — `[policy.isolation]` in company.toml picks per-role presets from the same 5 vetted phase manifests, reportable via `cast.roster_grant_report`; **OA2** (`lex-loom#183`) — those overrides are *enforced*, not just reported: the LLM executor's tool list is filtered through the Grant-derived manifest (`src/tool_grant.lex`) and `proc_cmd` nodes route through `lex-os exec` under the same resolved manifest when `LEX_OS_ISOLATION` is set; **OA3** (`lex-loom#184`, partial) — loom no longer hardcodes `--simulated` (opt-in `LEX_OS_SIMULATED`; unset defers to lex-os's real-by-default-on-KVM selection, refuse-don't-downgrade off KVM). Still open: reproducing the QA-deny/Build-allow proof against **real Firecracker** needs a KVM CI runner neither repo has (`lex-loom#184` stays open for exactly that); the A2A executor remains unmediated; loom's CI still doesn't install the `lex-os` binary. See `docs/design/lex-os-isolation.md` and `docs/design/soft-os-aware-agents.md` in lex-loom. |
| soft ↔ lex-os | **Not started.** No dependency declared. Its tools are already effect-scoped narrowly (`[net, io, proc]`, no direct `sql`), which is compatible with the grant model if this is ever built. |
| loom ↔ soft (direct) | **Wired (SA1–SA4, 2026-08-17): a real, declared runtime dependency.** loom's `lex.toml` now depends on `lex-soft`, and three integrations are live: **SA1** (`lex-loom#178`) — a `[soft]` section in company.toml (mesh_url, org_id, roles) threads through CompanyCfg into the board report; **SA3** (`lex-loom#180`) — a company's revenue readings route through soft's evidence-gated settlement (`src/soft_settlement.lex` records a claim and *independently re-verifies* it before the board report treats it as revenue); **SA4** (`lex-loom#181`) — outward-facing company roles register on soft's mesh as discoverable, A2A-messageable capabilities (`src/soft_register.lex`; the research role is live, `content_creator`'s write path deferred as `lex-loom#187`). The old claim ("no runtime dependency, lineage only") described the pre-#177 world; loom's vendored agent runtime is still its own — the dependency is on soft's mesh + settlement surface, not its runtime. |

Also worth noting: `lex-os`'s own README is accurate about its own limits —
it already states that `capsule install --run` provisions under the
*simulated* perimeter only (not a security boundary), tracked as
`lex-os#36`. That gap is real and orthogonal to this doc; it isn't specific
to hosting loom or soft.

## Answered since 2026-08-03

- *Should loom and soft integrate directly?* — **Answered: yes, and built.**
  A company's outward-facing roles now appear as A2A peers inside soft's
  mesh (SA4), and revenue settles through soft's evidence-gated settlement
  (SA3). The shared `lex-ctl` kernel was not enough on its own — the mesh
  registration and settlement surface turned out to be the integration the
  company layer actually needed.
- *If sandboxing becomes a priority, loom is the natural pilot* —
  **Confirmed: loom is the pilot.** Grant generation, per-role presets,
  declared overrides, and enforcement (tool filter + `lex-os exec`
  mediation) all landed (OA1–OA3). soft still has no manifest work.

## Open questions

- Real-KVM Firecracker validation of the loom↔os mediation
  (`lex-loom#184`) — blocked on a KVM CI runner; every claim above about
  the real perimeter is validated under `--simulated` plus lex-os's own
  unit/integration suites, not yet end-to-end from loom against real
  Firecracker.
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
