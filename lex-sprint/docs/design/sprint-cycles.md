# lex-sprint — design: multi-agent sprint cycles

> **Status.** Design proposal (v0.0). No implementation yet — this doc
> is the contract the first `src/` modules will be checked against.
>
> **Audience.** Agents and humans building on the Lex ecosystem
> (lex-llm, lex-agent, lex-spec, lex-trail, lex-jobs, lex-hub, lex-vcs,
> lex-code).
>
> **Grounding.** Built under [Trust Without Comprehension](https://alpibru.com/manifesto).
> This document cites the manifesto sections the existing repos already
> demonstrate: **§III** semantic (AST) diff, **§IV** honest effect rows,
> **§VI** effect-typed parallel orchestration, **§VIII** hash-chain
> tamper-evidence.

---

## 1. What we're building

A system that takes a project request and drives it end-to-end through
**sprint cycles**:

```
Intake → Design → Implementation → QA → Demo → Retro → Digest ─┐
   ↑                                                            │
   └──────────────── next sprint, seeded by Digest ─────────────┘
```

Two hard requirements shape the design:

1. **Dynamic topology.** The set of agents and the edges between them
   is *not* fixed up front. It is derived once the request is digested,
   and refined again after the Design phase produces details. The graph
   is an output of the system, not a constant in its source.

2. **Lex-native trust.** It must hold to the manifesto: an agent never
   has to *read and comprehend* another agent's output to trust it.
   Trust is mechanical — a typed contract, a spec precondition, an
   honest effect row, and a hash-chained attestation.

## 2. The central idea

The manifesto's load-bearing claim, quoted verbatim in `lex-code`'s own
§VI demo:

> *"the substrate carries the constraints; the model fills the bodies;
> the type system verifies the result."*

Applied to orchestration, this splits the system into two layers with
very different change rates and very different trust stories:

| Layer | Authored by | Changes | Trusted because |
|---|---|---|---|
| **The graph** — which agents, which edges, which gates | an LLM *Architect* agent, re-derived per request | every sprint | it is a *typed value* that validates against a schema + meta-spec, and is content-addressed (its own hash) |
| **The executor** — how any graph runs, what may flow | written once, in Lex | rarely | effect rows (§VI), spec gates evaluated at both ends, and an append-only trail (§VIII) |

All the dynamism the user asked for lives in layer 1 — and layer 1 is
**data, not code**: a `SprintGraph` value. Layer 2 is a fixed,
effect-typed interpreter of that value. This is the manifesto sentence
turned into an architecture: the substrate (layer 2) carries the
constraints; the model (the Architect) fills the bodies (the graph and
each node's work); the type checker verifies the result.

## 3. Reuse map — lex-sprint is mostly wiring

The ecosystem already provides every primitive. `lex-sprint` is the thin
orchestration layer that composes them.

| Dependency | Role in a sprint |
|---|---|
| **lex-llm** | Per-agent run loop + **spec-gated tool permissions** (`with_permission_gate`). A worker's tool access is a `Spec`, enforced at construction — not a prompt instruction. |
| **lex-agent** | Each role is an **A2A agent**: `AgentCard` + capabilities, each capability carrying a `Spec` precondition the server evaluates *before* the handler. Gets us interop with ADK / CrewAI / LangGraph agents on the same wire. Its **Task lifecycle** (`submitted/working/input-required/completed/canceled/failed`) with legal-transition gating (`tk.advance`) is the exact pattern we reuse for sprint *phase* transitions. Its SQLite `Store` makes tasks survive restarts. |
| **lex-spec** | The gates: between phases, and on every agent capability. SMT-checkable. The unit that the Digest phase *tightens* (§9). |
| **lex-schema** | Every handoff between nodes is a typed value with constraints, not free text — `ModelSchema` + `constraints`. |
| **lex-trail** | The hash chain (§VIII). Every proposal, handoff, gate decision and phase transition is appended; tamper shows up as a sequence gap. This **is** the sprint's memory and its proof. |
| **lex-vcs** (17 tools already wired in lex-code) | A branch per sprint; **semantic diff (§III)** of this sprint's graph/code against the previous one — including effect-row changes the line diff would miss. |
| **lex-jobs** | Durable queue so a long sprint survives process death: phase fan-out = enqueue N jobs; `Done / Retry(reason) / Fail(reason)` outcomes; `max_attempts`; `delay_seconds`. |
| **lex-hub** | Multi-tenant isolated store + JWT gateway — one project (or one sprint) per tenant, no cross-tenant access. |
| **lex-mcp** | Exposes the sprint controls (start, status, advance, inspect-trail) to Claude Code / Cursor as MCP tools. |
| **lex-code** | The existing worker agents — build / test / spec / review / plan / explore / refactor — and its `run_parallel` (`list.par_map`) is the literal seed of our orchestrator. |

What is genuinely **new** in lex-sprint: the `SprintGraph` value + its
validator, the phase state machine, the effect-typed graph executor, the
four orchestration roles (Architect / QA / Demo / Scribe), and the
Digest feedback loop.

## 4. Core types

The graph is data. The executor is the only thing that touches effects.

```lex
import "lex-spec/spec"     as spec
import "lex-schema/schema" as sch
import "lex-spec/capability" as cap

# A phase of the sprint. Transitions are gated (see §6).
type Phase =
  Intake | Design | Implementation | QA | Demo | Retro | Digest

# A node is one agent invocation. `role` resolves to either an in-process
# lex-llm AgentDef or a remote A2A AgentCard URL. `gate` is the spec that
# must hold for this node's *output* to be accepted.
type Node = {
  id         :: Str,
  role       :: Str,             # "architect" | "build" | "test" | ... | external URL
  capability :: cap.Capability,  # carries its own precondition spec
  gate       :: spec.Spec,       # postcondition on the node's typed output
}

# A directed, typed handoff. The producer's output must conform to
# `handoff` before it is delivered to `to`. Typed, not prose.
type Edge = {
  from    :: Str,
  to      :: Str,
  handoff :: sch.ModelSchema,
}

# The dynamic artifact. Content-addressed → has a stable hash → diffable
# across sprints (§III). Produced by the Architect, never hand-written.
type SprintGraph = {
  phase :: Phase,
  nodes :: List[Node],
  edges :: List[Edge],
}
```

## 5. The executor — one honest effect row (§VI)

The whole point of §VI is that the orchestrator declares *exactly* the
effects it composes, and the type checker rejects an under-declared row
at `lex check` time (lex-code already ships the negative-twin demo). Our
executor inherits that property:

```lex
# Runs one phase's sub-graph. Independent nodes fan out via list.par_map
# (the run_parallel pattern from lex-code); dependent nodes sequence by
# edge order. The declared row is the union of everything the body uses;
# the dishonest twin that drops, say, [llm] or [sql] is REJECTED.
fn run_phase(
  g   :: SprintGraph,
  ctx :: Ctx,
  db  :: Db,
  log :: trail.Log,
) -> [env, concurrent, net, llm, io, proc, sql, fs_write, time] PhaseResult {
  # 1. topological layering of g.nodes by g.edges
  # 2. for each layer: list.par_map over independent nodes
  # 3. for each node: invoke agent (lex-llm in-proc OR lex-agent client),
  #    validate output against the inbound edge's handoff schema,
  #    evaluate node.gate at BOTH ends (producer self-check + this check),
  #    append a trail entry (§VIII) regardless of accept/deny
  # 4. a denied gate is a hard stop for that edge — never a silent pass
}
```

Pure helpers (graph validation, topological sort, schema conformance)
carry **no** effect row and ship with `examples {}` blocks — free
regression tests folded into the SigId.

## 6. Phases as a gated state machine

Phase transitions reuse lex-agent's `tk.advance` discipline: illegal
moves are rejected, and **every legal move requires evidence**. You
cannot advance `QA → Demo` unless the QA gate produced an attestation;
the transition *consumes* that attestation. The legal table:

```
Intake         → Design
Design         → Implementation        (Architect graph validated)
Design         → Design                (re-plan: refined graph, §7)
Implementation → QA
QA             → Implementation         (gate failed: bounce back)
QA             → Demo                    (QA gate attested true)
Demo           → Retro
Retro          → Digest
Digest         → Intake                  (next sprint, seeded by §9)
```

Any other move raises `InvalidTransition`. There is no back-door
`Phase` construction — same rule lex-agent enforces for `Task`.

## 7. Dynamic topology & re-planning

This is the requirement that "the graph is defined once the request is
digested, or even after design for details."

1. **Derive after Intake.** The Architect agent reads the request and
   emits a `SprintGraph`. Before it ever runs, the executor validates it
   against (a) the `SprintGraph` schema and (b) a **meta-spec** (§8). An
   invalid graph is bounced back to the model with the structured error
   — it is *never* executed. The model fills the body; the substrate
   refuses a dishonest one.

2. **Refine after Design.** Once Design produces details, the Architect
   may emit a refined graph. The executor takes a **semantic diff (§III)**
   of `old` vs `new` graph and re-instantiates *only the changed nodes*
   — unchanged nodes keep their running state and their attestations.
   This is `lex diff` over a content-addressed value, not a text diff.

3. **External agents are first-class.** A node whose `role` is an A2A
   URL is resolved by fetching `/.well-known/agent.json` and calling it
   over JSON-RPC. The executor trusts it exactly as much as an in-process
   node: by its gate and its attestation, not by inspecting its code.

## 8. The meta-spec — what makes a graph executable

A `SprintGraph` is only run if it satisfies a fixed `Spec` (lex-spec,
SMT-checkable). Initial rules:

- Every `Node` has a non-trivial `gate` (no ungated output).
- Every `Edge.handoff` schema is satisfiable (lex-schema validation).
- The graph is a DAG **or** every cycle carries an explicit iteration
  budget (no unbounded agent loops).
- A `QA` node dominates every `Demo` node (you cannot demo unverified
  work).
- The composed effect row of all resolved nodes is a subset of the
  sprint's granted `--allow-effects` envelope (defense in depth on top
  of the type checker).

The meta-spec is itself versioned and attested; tightening it is a
deliberate, recorded act (§9).

## 9. Gates: evaluate-at-both-ends

Straight from lex-agent: the receiver never trusts the sender's gate.
Every handoff is checked twice — the producing node self-checks before
emitting, and the executor re-checks before delivering. A failure is a
**deny** (`Inconclusive` is also a deny — never a silent allow), surfaced
as the `-32099 spec-denied` extension code on the wire. Pairing this with
lex-llm's outbound-capability filter gives evaluate-at-both-ends by
construction.

## 10. Durability & multi-tenancy

A sprint is long-running and must survive restarts:

- **lex-jobs** backs each phase fan-out. Enqueuing N node-jobs and
  awaiting their acks *is* the phase join. `Retry` handles transient
  agent/network failure with `max_attempts`; `Fail` is terminal and
  recorded.
- **lex-agent** SQLite `Store` persists A2A task state across restarts.
- **lex-hub** isolates each project/sprint as a tenant (JWT `sub` →
  per-tenant store), so many sprints run without cross-contamination.

## 11. Audit & the learning loop (§VIII → next sprint)

Every event — graph proposed, graph validated/rejected, node started,
handoff accepted/denied, phase advanced, retro note, digest output — is
appended to a **lex-trail** hash chain. A deleted event shows up as a
sequence gap; the chain is tamper-evident. This chain is what lets the
*next* sprint trust the previous one without re-reading it.

The **Digest** phase (the *Scribe* role) reads the finished sprint's
trail + attestations + semantic diffs and emits **typed, attested**
artifacts:

- **Tightened Specs** — a QA miss this sprint becomes a *precondition*
  next sprint. The next agents trust a gate they didn't write.
- **Updated AgentCards / prompts** — capability descriptions refined
  from observed failures.
- **A seed `SprintGraph`** — the starting topology for the next cycle.

The crucial property: *learning is encoded into the substrate's
constraints, not stuffed into a prompt.* That is the manifesto's flywheel
— trust accrues mechanically across cycles.

## 12. Why this is not "CrewAI-in-Lex"

A conventional agent-graph framework gives you code + prose you must read
to trust. Here:

- the graph is a **typed, content-addressed value** (diffable, §III),
- every edge is a **schema-checked handoff** (no prose contracts),
- every node is **spec-gated and effect-typed** (§IV, §VI),
- every step is **append-only attested** (§VIII).

The executor can run agents it has never seen — including third-party
A2A agents — because trust is mechanical. That is the manifesto delivered
as a system rather than a slogan.

## 13. Module layout (for the first implementation PR)

```
lex-sprint/
  lex.toml
  src/
    graph.lex         # SprintGraph / Node / Edge ADTs + schema + validate (pure, examples{})
    phase.lex         # Phase + legal-transition table (tk.advance discipline)
    orchestrator.lex  # run_phase / run_sprint — the §VI honest effect row
    roles.lex         # Architect / QA / Demo / Scribe AgentDefs (extend lex-code modes)
    metaspec.lex      # the executable Spec a graph must satisfy (§8)
    digest.lex        # trail+attestations → tightened Specs + seed graph (§9, §11)
    server/
      a2a.lex         # A2A front door (start/advance/status as capabilities)
      mcp.lex         # lex-mcp tool surface
  tests/
    test_graph.lex    # validation + topo-sort property tests
    test_phase.lex    # legal / rejected transition table
    test_metaspec.lex # graph acceptance/denial cases
```

## 14. Deferred (explicit scope cuts)

- **Live SSE streaming of sprint progress** — depends on lex-agent's
  streaming write half (`lex-lang#487`); single-frame status until then.
- **OAuth/DID identity for cross-org sprints** — A2A ships
  unauthenticated for local dev in v0.1.
- **Cost/budget accounting per node** — the meta-spec reserves a hook
  (iteration budgets) but token-cost gating is a later milestone.
- **Human-in-the-loop approval gates** — modeled as an
  `input-required` phase pause; UI is out of scope for v0.

## 15. Milestones

- **M0 (this doc).** Design accepted.
- **M1.** `graph.lex` + `phase.lex` + `metaspec.lex` with full
  `examples {}` / tests; no live agents — graphs validated and
  transitions checked offline.
- **M2.** `orchestrator.lex` running in-process lex-llm worker nodes for
  one full Intake→Demo pass on a toy request; trail recorded.
- **M3.** `roles.lex` Architect emitting real `SprintGraph`s; re-plan
  via semantic diff.
- **M4.** `digest.lex` closing the loop — sprint N+1 seeded by sprint N.
- **M5.** Durability (lex-jobs) + multi-tenancy (lex-hub) + MCP/A2A
  front door.

---

Built under the principles of [Trust Without Comprehension](https://alpibru.com/manifesto).
