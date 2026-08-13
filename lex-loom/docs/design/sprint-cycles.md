# lex-loom — design: multi-agent sprint cycles

> **Status.** Design proposal (v0.0). No implementation yet — this doc
> is the contract the first `src/` modules will be checked against.
>
> **Name.** The package is `lex-loom` — the orchestration substrate.
> The workflow it runs is a **sprint cycle**. A loom holds a fixed
> **warp** (the effect-typed substrate and the gates) and a dynamic
> **weft** (the agent bodies the model fills in) and weaves them into one
> verifiable fabric — which is exactly the §VI claim below.
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

All the dynamism the request demands lives in layer 1 — and layer 1 is
**data, not code**: a `SprintGraph` value. Layer 2 is a fixed,
effect-typed interpreter of that value. This is the manifesto sentence
turned into an architecture: the substrate (layer 2) carries the
constraints; the model (the Architect) fills the bodies (the graph and
each node's work); the type checker verifies the result.

## 3. Reuse map — lex-loom is mostly wiring

The ecosystem already provides every primitive. `lex-loom` is the thin
orchestration layer that composes them.

| Dependency | Role in a sprint |
|---|---|
| **lex-llm** | Per-agent run loop + **spec-gated tool permissions** (`with_permission_gate`). A worker's tool access is a `Spec`, enforced at construction — not a prompt instruction. |
| **lex-agent** | Each role is an **A2A agent**: `AgentCard` + capabilities, each capability carrying a `Spec` precondition the server evaluates *before* the handler. Gets us interop with ADK / CrewAI / LangGraph agents on the same wire. Its **Task lifecycle** (`submitted/working/input-required/completed/canceled/failed`) with legal-transition gating (`tk.advance`) is the exact pattern we reuse for sprint *phase* transitions. Its SQLite `Store` makes tasks survive restarts. |
| **lex-spec** | The gates: between phases, and on every agent capability. SMT-checkable. The unit that the Digest phase *tightens* (§12). |
| **lex-schema** | Every handoff between nodes is a typed value with constraints, not free text — `ModelSchema` + `constraints`. |
| **lex-trail** | The hash chain (§VIII). Every proposal, handoff, gate decision and phase transition is appended; tamper shows up as a sequence gap. This **is** the sprint's memory and its proof. |
| **lex-vcs** (17 tools already wired in lex-code) | A branch per sprint; **semantic diff (§III)** of this sprint's graph/code against the previous one — including effect-row changes the line diff would miss. The artifact plane (§4). |
| **lex-jobs** | The **durable work queue** — the control-plane spine (§4): phase fan-out = enqueue N jobs; `Done / Retry(reason) / Fail(reason)` outcomes; `max_attempts`; `delay_seconds`. |
| **lex-hub** | Multi-tenant isolated store + JWT gateway — one project (or one sprint) per tenant, no cross-tenant access. |
| **lex-mcp** | Exposes the sprint controls (start, status, advance, inspect-trail) to Claude Code / Cursor as MCP tools. |
| **lex-code** | The existing worker agents — build / test / spec / review / plan / explore / refactor — and its `run_parallel` (`list.par_map`) is the literal seed of our orchestrator. |

What is genuinely **new** in lex-loom: the `SprintGraph` value + its
validator, the phase state machine, the effect-typed graph executor, the
four orchestration roles (Architect / QA / Demo / Scribe), and the
Digest feedback loop.

## 4. Communication architecture — four planes

How do the agents actually talk? Not over one bus. The instinct from the
queue-based predecessor (lex-soft) is **kept and made the spine** — but
"a queue" vs "git + webhooks" is a false choice: each kind of message
rides the substrate that fits it. Four planes:

| Plane | What flows | Substrate | Why this one |
|---|---|---|---|
| **Control / work** | "run this node", "here's the ack", retries, scheduling | **Durable queue — `lex-jobs`** | Long-running sprints must survive restarts. Phase fan-out = enqueue N node-jobs; phase **join** = await acks. At-least-once delivery, `Retry`/`Fail`, `max_attempts`, `delay_seconds`, backpressure, horizontal worker scaling. This is the lex-soft queue intuition, kept. |
| **Inter-agent calls / interop** | "agent X asks agent Y, typed, and waits" | **A2A — `lex-agent`** | JSON-RPC + AgentCards make capabilities discoverable; spec preconditions gate **at both ends**; SSE streams progress; and it is the wire that talks to *external* ADK / CrewAI / LangGraph agents. The call protocol, layered on top of the queue for delivery. |
| **Artifacts** (code, designs, the `SprintGraph` itself) | the actual deliverables | **git / `lex-vcs`** — one branch per sprint | Handoffs carry a **content hash / ref**, never the payload. Artifacts are content-addressed, so the hash *is* the proof and semantic diff (§III) just works. Git's right job: the artifact + merge surface, **not** the message bus. |
| **Events / audit / coordination** | "this gate passed", "phase advanced" | **`lex-trail`** hash chain (+ SSE/webhooks as *egress* only) | The immutable record of what happened (§VIII); drives coordination internally. Webhooks/SSE are an *outbound notification edge* to humans and external systems — not the internal bus. |

**One handoff, end to end:**

1. The orchestrator **enqueues** a job on the `role:build` queue
   (`lex-jobs`). The payload is *small*: the typed handoff record + a
   content-hash pointer to inputs + a trail-seq pointer — not the files.
2. A build worker (an `lex-agent` A2A agent wrapping an `lex-llm` loop)
   **pulls** the job, fetches inputs from `lex-vcs` by hash, does the
   work, and writes the artifact back to the sprint branch (new hash).
3. It self-checks its `gate` spec, **appends** a result entry to
   `lex-trail`, and `Done`-acks the job. The orchestrator **re-checks**
   the gate (evaluate-at-both-ends, §10) before delivering downstream.

**Why not "git + webhooks" as the bus?** Webhooks are lossy, unordered,
not durable, have no retry/backpressure, and are awkward for fan-in
joins; coupling the control plane to a forge is a liability. Git-as-bus
(commit = message) serializes through refs, fights merge contention, and
conflates artifacts with control. Git is *excellent* for artifacts and
*bad* for control messaging — so it carries exactly the former.

**Topology of execution.** v0 uses a **central orchestrator** driving the
graph (one effect-typed `run_phase`, one place for the meta-spec and the
effect row) with **pull-based** workers (queue gives backpressure). Pure
choreography — each agent enqueues its own successors per the graph — is
a later optimization; because the graph is *data*, the execution strategy
can change without touching the agents.

**Open fork (deferred).** The queue could be *collapsed into the trail*:
event-source the work bus as "append intent to the hash chain, workers
tail it," unifying transport and audit. Elegant and very on-manifesto,
but more to build than reusing `lex-jobs`. Kept separate for v0.

## 5. Core types

The graph is data. The executor is the only thing that touches effects.

```lex
import "lex-spec/spec"     as spec
import "lex-schema/schema" as sch
import "lex-spec/capability" as cap

# A phase of the sprint. Transitions are gated (see §7).
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
# `handoff` before it is delivered to `to`. Typed, not prose. On the wire
# this travels as a content-hash ref (§4), not the payload itself.
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

## 6. The executor — one honest effect row (§VI)

The whole point of §VI is that the orchestrator declares *exactly* the
effects it composes, and the type checker rejects an under-declared row
at `lex check` time (lex-code already ships the negative-twin demo). Our
executor inherits that property:

```lex
# Runs one phase's sub-graph. Independent nodes fan out via the queue
# (§4) / list.par_map; dependent nodes sequence by edge order. The
# declared row is the union of everything the body uses; the dishonest
# twin that drops, say, [llm] or [sql] is REJECTED.
fn run_phase(
  g   :: SprintGraph,
  ctx :: Ctx,
  db  :: Db,
  log :: trail.Log,
) -> [env, concurrent, net, llm, io, proc, sql, fs_write, time] PhaseResult {
  # 1. topological layering of g.nodes by g.edges
  # 2. for each layer: enqueue independent node-jobs, await acks (§4)
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

## 7. Phases as a gated state machine

Phase transitions reuse lex-agent's `tk.advance` discipline: illegal
moves are rejected, and **every legal move requires evidence**. You
cannot advance `QA → Demo` unless the QA gate produced an attestation;
the transition *consumes* that attestation. The legal table:

```
Intake         → Design
Design         → Implementation        (Architect graph validated)
Design         → Design                (re-plan: refined graph, §8)
Implementation → QA
QA             → Implementation         (gate failed: bounce back)
QA             → Demo                    (QA gate attested true)
Demo           → Retro
Retro          → Digest
Digest         → Intake                  (next sprint, seeded by §12)
```

Any other move raises `InvalidTransition`. There is no back-door
`Phase` construction — same rule lex-agent enforces for `Task`.

## 8. Dynamic topology & re-planning

This is the requirement that "the graph is defined once the request is
digested, or even after design for details."

1. **Derive after Intake.** The Architect agent reads the request and
   emits a `SprintGraph`. Before it ever runs, the executor validates it
   against (a) the `SprintGraph` schema and (b) a **meta-spec** (§9). An
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
   over JSON-RPC (§4). The executor trusts it exactly as much as an
   in-process node: by its gate and its attestation, not by inspecting
   its code.

## 9. The meta-spec — what makes a graph executable

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
deliberate, recorded act (§12).

## 10. Gates: evaluate-at-both-ends

Straight from lex-agent: the receiver never trusts the sender's gate.
Every handoff is checked twice — the producing node self-checks before
emitting, and the executor re-checks before delivering. A failure is a
**deny** (`Inconclusive` is also a deny — never a silent allow), surfaced
as the `-32099 spec-denied` extension code on the wire. Pairing this with
lex-llm's outbound-capability filter gives evaluate-at-both-ends by
construction.

## 11. Durability & multi-tenancy

A sprint is long-running and must survive restarts. This is the control
plane of §4, made durable:

- **lex-jobs** backs each phase fan-out. Enqueuing N node-jobs and
  awaiting their acks *is* the phase join. `Retry` handles transient
  agent/network failure with `max_attempts`; `Fail` is terminal and
  recorded. *(Caveat: lex-jobs v0.1 has a documented multi-worker race
  on Postgres — needs `SELECT … FOR UPDATE SKIP LOCKED`; fine on SQLite
  / a single orchestrator for v0.)*
- **lex-agent** SQLite `Store` persists A2A task state across restarts.
- **lex-hub** isolates each project/sprint as a tenant (JWT `sub` →
  per-tenant store), so many sprints run without cross-contamination.

## 12. Audit & the learning loop (§VIII → next sprint)

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

## 13. Why this is not "CrewAI-in-Lex"

A conventional agent-graph framework gives you code + prose you must read
to trust. Here:

- the graph is a **typed, content-addressed value** (diffable, §III),
- every edge is a **schema-checked handoff** (no prose contracts),
- every node is **spec-gated and effect-typed** (§IV, §VI),
- every step is **append-only attested** (§VIII).

The executor can run agents it has never seen — including third-party
A2A agents — because trust is mechanical. That is the manifesto delivered
as a system rather than a slogan.

## 14. Module layout (for the first implementation PR)

```
lex-loom/
  lex.toml
  src/
    graph.lex         # SprintGraph / Node / Edge ADTs + schema + validate (pure, examples{})
    phase.lex         # Phase + legal-transition table (tk.advance discipline)
    orchestrator.lex  # run_phase / run_sprint — the §VI honest effect row
    transport.lex     # the four planes (§4): enqueue/await over lex-jobs, A2A client,
                      #   artifact ref get/put over lex-vcs, trail append
    roles.lex         # Architect / QA / Demo / Scribe AgentDefs (extend lex-code modes)
    metaspec.lex      # the executable Spec a graph must satisfy (§9)
    digest.lex        # trail+attestations → tightened Specs + seed graph (§12)
    server/
      a2a.lex         # A2A front door (start/advance/status as capabilities)
      mcp.lex         # lex-mcp tool surface
  tests/
    test_graph.lex    # validation + topo-sort property tests
    test_phase.lex    # legal / rejected transition table
    test_metaspec.lex # graph acceptance/denial cases
```

## 15. Deferred (explicit scope cuts)

- **Live SSE streaming of sprint progress** — depends on lex-agent's
  streaming write half (`lex-lang#487`); single-frame status until then.
- **OAuth/DID identity for cross-org sprints** — A2A ships
  unauthenticated for local dev in v0.1.
- **Cost/budget accounting per node** — the meta-spec reserves a hook
  (iteration budgets) but token-cost gating is a later milestone.
- **Human-in-the-loop approval gates** — modeled as an
  `input-required` phase pause; UI is out of scope for v0.
- **Choreographed (decentralized) execution** and **trail-as-work-bus**
  (§4 open fork) — both v2.

## 16. Milestones

- **M0 (this doc).** Design accepted.
- **M1.** `graph.lex` + `phase.lex` + `metaspec.lex` with full
  `examples {}` / tests; no live agents — graphs validated and
  transitions checked offline.
- **M2.** `orchestrator.lex` + `transport.lex` running in-process
  lex-llm worker nodes for one full Intake→Demo pass on a toy request;
  trail recorded, artifacts by reference.
- **M3.** `roles.lex` Architect emitting real `SprintGraph`s; re-plan
  via semantic diff.
- **M4.** `digest.lex` closing the loop — sprint N+1 seeded by sprint N.
- **M5.** Durable queue (lex-jobs) + multi-tenancy (lex-hub) + MCP/A2A
  front door across processes.

---

Built under the principles of [Trust Without Comprehension](https://alpibru.com/manifesto).
