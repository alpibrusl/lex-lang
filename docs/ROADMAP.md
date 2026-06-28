# Lex — Project Roadmap

> **Scope.** This is the *cross-repo* roadmap for the whole Lex project —
> the substrate (`lex-lang`), the runtime (`lex-os`, `lex-os-manifest`),
> the spec layer (`lex-spec`), and the applications (`lex-robot`,
> `lex-loom`). It lives in `lex-lang` because that is the substrate root.
> Per-repo detail stays in each repo's own `STATUS.md` / `DESIGN.md` /
> `PLATFORM.md`; this file is the sequencing across them.
>
> Status date: 2026-06. This is a map, not a contract — the code is the
> truth. Issue numbers refer to `alpibrusl/lex-lang` unless prefixed.

---

## 1. The thesis (what every repo is an instance of)

Strip away the surface and every Lex repo is the same three primitives:

1. **Capability gating** — an illegal action is refused *before any logic
   runs*. Effects-as-types in `lex-lang`; the `Grant` in `lex-os`;
   `Capability.gate → Verdict` in `lex-spec`; workspace/force clamps in
   `lex-robot`; `op_grant`/`op_call` in `lex-loom`.
2. **Hash-chained trails** — every accepted action is appended to a
   content-addressed log; tamper with a field and the id stops
   recomputing.
3. **Replay-verification** — the outcome is *re-derived from the trail by
   the rules*, never trusted from the actor.

The strategic asset is that this triad is implemented *consistently* four
layers deep — language, runtime, and two application domains. The
strategic risk is that breadth (four application surfaces) is currently
outrunning proof (no frozen substrate contract, no independently-verified
external user). **This roadmap exists to correct that order: freeze the
contract, build the kernel, prove one wedge — before going wider.**

---

## 2. Where each repo stands today

| Repo | Maturity | One-line status |
|---|---|---|
| **lex-lang** | v0.10.4, ~93k SLOC, 16 crates | Production-grade core: effect sandbox proven (7/7 adversarial), VCS tier-2 shipping. JIT is a phase-1 MVP. |
| **lex-os** | ~8.8k SLOC | Real Firecracker microVM perimeter (not stubbed) + host egress wall; simulated backend is the honest portable default. |
| **lex-os-manifest** | v0.1, ~278 lines | Trust lattice / grant / budget / reversibility as pure Lex. Complete, minimal. |
| **lex-spec** | v0.1, ~1.5k lines | Spec DSL + evaluator + SMT-LIB export + property check. Shells out to Z3; existentials/composition deferred. |
| **lex-robot** | ~16k LoC, prototype | Tier-3 reaches real Unitree G1 kinematics in MuJoCo (contact + weld). Motion scripted; no hardware; microVM pending. |
| **lex-loom** | P0–P1b + C1–C6 shipped | Most mature application: four-layer independent sprint verifier (integrity → grounded gates → authority → operations). |

Engineering discipline is high across all six: strict CI, migration docs
on breaking changes, gaps documented rather than hidden
(`lex-lang/docs/INVARIANTS.md` explicitly lists what is *not yet* a
contract).

---

## 3. The three gaps that gate everything else

1. **The core contracts are not frozen.** SigId/OpId hash stability is not
   yet a committed contract (#560); the effect-boundary property is
   *demonstrated, not proven* (#614). Until both are settled, every
   downstream pin is a liability — the 0.10.0 breaking stdlib change is the
   live example.
2. **There is a substrate, but no kernel.** Reputation is recomputed
   per-manifest, not *owned* by a durable agent identity; tokens/grants are
   hardcoded in examples; verification runs only locally; settlement is
   mocked. (See `lex-robot/docs/PLATFORM.md` § "What's missing".)
3. **Demo sprawl.** Four application surfaces (robots, agent companies,
   games, commerce), none with a load-bearing *external* user. One proven
   third-party verification beats ten internal demos.

---

## 4. Roadmap — three phases, in order

The phases are sequential by dependency, not by preference. Phase 1
unblocks 2; a credible Phase 3 wedge depends on both.

### Phase 1 — Freeze the substrate contract  *(prerequisite for all external adoption)*

The substrate cannot ask anyone to build on content-addresses that might
shift, or to trust a boundary that is only demonstrated.

- [ ] **Commit to SigId / OpId / StageId stability (#560).** Promote the
      relevant sections of `docs/INVARIANTS.md` from "policy" to a
      *versioned contract* with an explicit compatibility statement. Gate
      it in CI with the existing `conformance` crate extended to a frozen
      golden corpus that must never change without a major version bump.
- [ ] **Make the boundary property provable, not just demonstrated
      (#614).** Turn the 7/7 adversarial sandbox result into a CI-gated
      *property*: a fuzz corpus (`fuzz/`) asserting "no body executes an
      effect outside its declared row," run as an invariant rather than a
      one-shot demo. Stretch: a machine-checked argument for the core
      effect-soundness lemma.
- [ ] **Resolve the documented non-contracts** that block durability:
      bytecode version tag, NodeId stability across edits, `body_hash`
      cross-process meaning (`docs/INVARIANTS.md` §gaps). Decide per item:
      promote to contract, or document permanently as transient.
- [ ] **Stabilize the stdlib surface enough to pin.** Land the remaining
      audit items (#681) and declare a stdlib-stability window so
      downstream repos can pin a `lex` version for more than a few days.

**Exit criterion:** a downstream repo can pin a `lex` version and a SigId
and trust both across the next several releases, with a written contract
saying so.

### Phase 2 — Build the platform kernel  *(cross-cutting; unblocks every app)*

Not a fifth application — the three missing primitives that every existing
app already needs. Sourced from `lex-robot/docs/PLATFORM.md`.

- [ ] **Durable agent identity + portable reputation.** A `did:lex`
      identity an agent *owns* and carries across sessions and apps; an
      agent registry; reputation that accrues to the identity, not to a
      per-manifest recomputation. (`lex-robot` already has the
      reputation-from-verified-trail logic — lift it to a durable,
      cross-app identity.)
- [ ] **A control plane.** Issue / scope / revoke capability + budget
      tokens; review trails. Today these are hardcoded in examples. This is
      the smallest piece with the largest leverage — every app stops
      hardcoding grants.
- [ ] **Hosted verify-as-a-service + trust anchoring.** The whole value
      prop is "hand a third party a trail, they re-derive the outcome."
      That only happens if there is a hosted replay endpoint and anchored
      trail roots. `lex-loom`'s P0–P1b verifier and `lex-robot`'s
      `robot_task` referee are the same shape — host them.
- [ ] **Real settlement (stretch).** Wire the real `lex-guard` Solana
      `exact` leg into the Magentic Bazaar in place of the x402 mock.

**Exit criterion:** an agent has an identity and reputation it carries
between two different apps, and a third party can verify a trail without
running it locally.

### Phase 3 — Prove one flagship wedge externally  *(narrow, don't widen)*

Pick **one** application and make it externally runnable end-to-end
against the hosted verifier. Recommendation: **lex-loom** —
"verifiable autonomous software delivery you don't have to trust" is the
most defensible buyer-facing story and the most mature app.

- [ ] One end-to-end `lex-loom` company that a third party runs, whose
      verdict they confirm independently via the Phase-2 hosted verifier.
- [ ] Packaged onboarding: a "bring your agent in 5 minutes" SDK / template
      (closes the SDK gap in `PLATFORM.md`).
- [ ] One named external pilot user. One is the milestone.

**Exit criterion:** someone outside the project has run the wedge and
independently verified a result they did not produce.

---

## 5. Explicitly *not* now (deferred with reason)

These are real and tracked, but downstream of the three phases above.
Doing them first widens surface without closing the trust gap.

| Deferred | Why it waits |
|---|---|
| JIT slices 2–5 / value-repr rework (#465) | Performance, not adoption. Core is already competitive. |
| VCS tier-3 federation (#173) | Needs the stability contract (Phase 1) first. |
| In-process Z3 (`lex-spec`, JIT slice 5) | Linking/WASM complexity; Z3-over-shell is fine for now. |
| Tier-4 robot hardware (`lex-robot`) | Needs microVM-on-Linux (lex-os) + firmware-floor work; not the trust bottleneck. |
| Further `lex-loom` company features (C7+) | Phase 3 is *external proof* of what exists, not more features. |
| Additional application surfaces (games, commerce expansion) | Demo sprawl is the diagnosed problem, not the cure. |

---

## 6. One-sentence summary

The language and runtime are ready; the **substrate contracts** and the
**platform kernel** are not — and those, not more applications, are the
gate to everything the project is trying to do next.
