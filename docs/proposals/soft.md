# Soft Integration: A Framework for Goal-Directed, Capability-Bounded Multi-Agent Systems

| Field | Value |
|---|---|
| Status | Draft |
| Owner | TBD |
| Depends on | lex-vcs tier-2 (#128) |
| Target home | Dedicated repo in `alpibrusl/` org (working name: `soft`) |
| Last revised | 2026-05-06 |

## Summary

A *soft-integrated* system is a set of autonomous agents that communicate **only when needed to advance their declared goals** — no central scheduler, no broadcast bus, no periodic polling. Each agent owns its state, declares its goals as invariants, holds an explicit list of peers it may contact, and is bounded at the type level by the effects it may perform. This document proposes a framework, expressed as a Lex stdlib + Rust runtime + transport adapters, that makes this pattern the path of least resistance to build in. The framework is domain-neutral: the canonical examples here are producer/consumer and sensor/actuator coordination, not any specific industry.

## Motivation

Most "distributed system" architectures in the wild fall into one of three camps:

1. **Central system, periodic ingest.** Every component reports state to a central system every N seconds; the central system makes all decisions. Simple, but: decisions lag the period, the central system is a single point of failure, every component's data is on the wire whether it matters or not, and the central system needs domain knowledge of every component it coordinates.
2. **Event-driven architecture.** A bus carries every event; subscribers react. Decouples producers from consumers but makes "who decided what, why" almost unanswerable, and the bus itself becomes a coordination point with its own scaling problems.
3. **Microservices with synchronous APIs.** Each service exposes endpoints; callers invoke them directly. High call frequency, tight coupling at the call-graph level, no built-in notion of "I only call you when my goal requires it."

A fourth pattern is older than all three but underused outside research and Erlang shops: agents that hold state, pursue goals, and talk only when they must. The classic name is **multi-agent systems**, sometimes phrased as **BDI** (Belief / Desire / Intention) agents or as **actor systems with intent**. We're calling our flavor *soft integration* because the integration glue is soft (sparse, intermittent, locally-decided) rather than hard (centralized, periodic, globally-coordinated).

The framework's contribution is not the pattern — it's the substrate. Specifically, that two of the hardest things about agent systems become structural rather than ad-hoc when expressed in Lex:

- **Capability bounds.** "Truck-agent may only contact tms-agent and csms-agent, may emit `[net]` and `[time]` only" is a *type signature* and a deploy-time policy, not a YAML file in a service mesh.
- **Goal invariants.** "Consumer never exceeds capacity" is a `spec` block proved by the random+SMT checker, attached to the agent definition. Violations are caught before a decision is emitted, not in a postmortem.

The framework gets these for free from Lex; everything else (mailbox, scheduler, transport, audit) is plumbing.

## Non-goals

Declared explicitly so contributors don't drift:

1. **No central scheduler.** If the answer to "how does X happen?" is "ask the central planner," the framework is being misused. Use a different pattern.
2. **No broadcast bus.** Agents have explicit peer lists. If you need broadcast, you're modeling something else and should reach for Kafka / NATS directly.
3. **No multi-language support.** Soft is Lex-only by design. The capability bounds depend on Lex's effect system; canonical message identity depends on Lex's canonical AST. Polyglot support is a future-extract concern, not a v1 feature.
4. **No LLM-in-the-loop primitive.** LLMs *write* agents (via `lex agent-tool`) but are not part of the runtime. The runtime is deterministic; the only nondeterminism is incoming messages and time.
5. **No real-time guarantees.** Soft real-time only (10ms-class latency targets). Hard real-time loops (sub-millisecond, jitter-bounded) live below the agent boundary, in Rust or C, with the agent calling them via a native binding.
6. **No general-purpose actor framework.** Soft is opinionated: typed messages, declared peers, declared goals. If you want untyped Erlang-style mailboxes with arbitrary message shapes, use Erlang.
7. **No pretense of being a service mesh replacement.** Soft is the application-level pattern that runs *on top of* whatever transport you have. Mesh concerns (mTLS, load-balancing, retry policy) are below Soft.

## Concepts

### Agent

An autonomous, long-running, stateful unit. Has:

- **Identity.** Content-addressed (Blake3 of the canonicalized agent definition + initial config). Same shape = same identity hash, across restarts and across machines.
- **State.** Agent-local. Transitions are immutable: each handler returns a new state, never mutates. The runtime persists state via lex-vcs operations; the agent code never touches storage directly.
- **Goals.** A list of `spec` references. Each spec is a Lex behavioral contract the agent commits to maintain. The runtime checks all goals before emitting any decision; a violation rejects the decision.
- **Peers.** An explicit list of `peer.Ref` values the agent may address. Off-list addresses are unreachable by capability — the runtime doesn't even attempt delivery.
- **Mailbox.** Typed inbox. One handler function per declared message type. Untyped messages are rejected at the transport boundary, before reaching the handler.
- **Effects.** Declared at the agent level (e.g., `[net, time]`). Constrains every handler. A handler that wants more effects than the agent declared fails type-check.

### Peer

An addressable agent reference. Resolves to a *transport address* + the *agent identity hash* the address is expected to have at that endpoint. Two peers with the same identity hash are the same agent regardless of where they happen to run; address mismatches are reported as drift, not silently routed.

### Message

A typed value with a content-addressed schema. Two agents that "agree" on a message type — i.e., import the same canonical shape — produce the same `SigId` for that type. Schema drift is structurally visible: an agent compiled against `Announce v1` cannot call a peer compiled against `Announce v2` without a typed conversion, and the runtime reports the mismatch by hash, not by error string.

### Goal

A `spec` block referenced from the agent definition. The runtime treats goals as decision-emission predicates: every action the agent emits (Send, Persist, Schedule, etc.) is checked against the conjunction of goals; if any goal is violated by the proposed action, the action is rejected and the runtime emits a structured violation record to the audit log.

Goals are *commitment-shaped*, not invariant-shaped: "no item older than its deadline is dropped" is a goal; "the queue is sorted" is not (it's a representation invariant, belongs in the type, not in the goal layer).

### Capability

The pair `(declared effects, declared peers)`. A capability is verified statically (effect type-checking against the agent's declared set) and at deploy time (the runtime's `--allow-effects` and `--allow-peers` policies must permit the agent's declarations). An agent that exceeds its capability cannot run, regardless of what its handlers want to do.

### Decision

A handler returns `(NewState, [Action])`. Actions are typed: `Send(peer, msg)`, `Schedule(when, msg)`, `Persist(checkpoint)`, `Emit(metric)`, `Stop`. The runtime owns the loop; agent code is pure with respect to side effects, which only occur when the runtime executes returned Actions after checking goals and capabilities.

## Architecture

```
┌─────────────────────────────────────────────────┐
│ Agent code                                      │  Lex handlers (user-written)
├─────────────────────────────────────────────────┤
│ Goal/spec gate                                  │  Pre-emit: are goals upheld?
├─────────────────────────────────────────────────┤
│ Capability gate (effects + peers)               │  Static + deploy-time
├─────────────────────────────────────────────────┤
│ Mailbox + scheduler                             │  Typed inbox, decision loop
├─────────────────────────────────────────────────┤
│ Transport adapter                               │  in-proc / HTTP / WS / NATS
├─────────────────────────────────────────────────┤
│ Audit + operation log                           │  lex-vcs (tier-2, #128)
└─────────────────────────────────────────────────┘
```

The bottom layer is *literally* lex-vcs tier-2. Every agent decision becomes an Operation (#129) tagged with an Intent (#131). The audit log is just a predicate query over the operation log (#133): `Intent.kind == "soft-decision" AND agent_id == X`.

This is a deliberate, load-bearing alignment: the soft framework is the highest-leverage user of lex-vcs tier-2, and lex-vcs tier-2 is the audit backbone of the soft framework. Building either without the other in mind is leaving structure on the floor.

## Agent lifecycle

1. **Definition.** Lex source declares the agent's state type, message types, handlers, goals, peers, effects. Compiled to a content-addressed agent definition (`AgentSig`).
2. **Spawn.** The runtime instantiates the agent with an initial state. Spawn writes a `SpawnAgent` operation to the audit log, including the `AgentSig`, the resolved peer addresses, and the declared capabilities.
3. **Receive → Decide → Emit.** For each incoming message:
   - Transport delivers a typed value matching a declared message type. Mismatches are rejected at the boundary.
   - The matching handler runs against current state, returning `(NewState, [Action])`.
   - The goal gate evaluates each Action against the agent's specs. Failed actions are dropped, recorded as `GoalViolation` in the audit log.
   - The capability gate verifies each surviving Action against the deploy policy. Out-of-capability actions are rejected the same way.
   - Surviving Actions are executed in order. Each execution writes a `Decision` operation to the audit log with a back-reference to the originating message.
4. **Persist.** State updates are committed via lex-vcs operations. The agent's state at any point is reproducible by replaying its operation log.
5. **Stop.** Triggered by an internal `Stop` action or external signal. Writes a `StopAgent` op; releases peer registrations.

The lifecycle is deterministic up to the order of incoming messages and the values returned by `[time]` and `[rand]` effects. `lex replay` against an agent's operation log reproduces every decision exactly given the same inbound message order.

## Protocol and addressing

### Wire format

Messages on the wire are `(envelope, payload)`:

```
envelope = {
  from: peer.Ref,
  to: peer.Ref,
  msg_type: SigId,           # canonical type identity
  msg_hash: Hash,            # content hash of the payload
  intent_id: Option<IntentId>,  # propagated for causal chains
  causal_parents: [MsgId],   # messages this one assumes
  timestamp: Time,
}
payload = canonical-JSON of the typed value
```

`msg_type` is a `SigId`, not a string name, so name collisions across agents are impossible — two agents either agree on the canonical shape or they don't.

### Discovery

Three modes, increasing complexity:

1. **Static.** Peer addresses configured at agent spawn. Sufficient for systems with fixed topology.
2. **Registry.** A small `soft-registry` service that holds `agent_id → address` mappings. Agents register on spawn, query on need. Registry calls are gated by `[net]` + `--allow-net-host registry.example.com`.
3. **Federated.** Multiple registries with gossip. Out of scope for v1.

### Causality

Each message carries `causal_parents` — the message IDs the sender's decision depended on. The audit log is a causal DAG over messages and decisions, not a flat timeline. This makes "why did agent X decide Y at time T?" answerable by walking parents, not by guessing from timestamps.

## Capability model

The capability of an agent is the pair `(effects, peers)`.

### Effects

Inherit from Lex's effect system. An agent declared `effects: [net, time]` cannot have a handler that performs `[fs_write]`. Type-check rejects it before the agent can spawn.

### Peers

A new dimension on top of effects. The `[net]` effect alone is too permissive: it says "this agent talks to the network" but doesn't say *to whom*. Soft adds a peer dimension to the runtime policy:

```
soft run agent.lex \
  --allow-effects net,time \
  --allow-peers tms,csms \
  --peer tms=https://tms.example.com:443/agent \
  --peer csms=https://csms.example.com:443/agent
```

`--allow-peers` is a whitelist over the agent's declared peer references. An agent compiled with `peers: [tms, csms]` deployed with `--allow-peers tms` only sees tms; calls to csms fail the capability gate at decision time.

This is structurally finer-grained than `--allow-net-host`: a peer ref binds an agent identity, not just a hostname. If `tms`'s address changes, the peer ref still points to the same agent identity; the runtime updates the address resolution.

### Verification

Capability checks happen at three points:

1. **Compile time.** Effect signatures are verified by the existing Lex type checker.
2. **Spawn time.** The runtime compares the agent's declared capability against the deploy policy. Mismatches refuse to spawn.
3. **Decision time.** Each emitted Action is checked against the capability before execution. This catches the case where the deploy policy changes mid-life of an agent.

## Goal layer

Goals are specs. The framework reuses the existing `lex spec check` infrastructure (#10 in the README's milestone table) — random property checking + SMT-LIB export.

### Binding goals to agents

```
agent consumer {
  state: ConsumerState,
  effects: [net, time],
  peers: [producer],
  handlers: { Announce: on_announce, ... },
  goals: ["specs/no_overflow.spec", "specs/responds_within_5s.spec"],
}
```

Each goal is a separate spec file. The runtime evaluates them per Action, not per handler — a single handler that emits 3 Actions has each Action gated independently.

### Goal violations

When a goal rejects an Action:

1. The Action is dropped (not executed).
2. A `GoalViolation` operation is written to the audit log, including the violating Action, the rejecting goal's `SpecId`, the counterexample if SMT produced one, and the agent's state at decision time.
3. The handler's other Actions (in the same return list) are still attempted; one Action's rejection does not invalidate the others, since they're declared independently.
4. The agent's state is *not* rolled back. Handlers are pure; if the new state itself violates a state-shaped invariant, that's a different kind of check (a representation invariant, not a goal — out of scope here).

This is intentionally tolerant: rejecting a single Action lets the agent continue operating, possibly with a degraded mode. Aggressive rollback or agent-suicide on violation is too brittle for systems where goals can conflict.

### Goal evolution

Goals can change between versions of an agent. The audit log records which goal set was in force at every decision, so "this agent stopped responding to anomalies in March" is answerable by diffing goal sets across spawn ops.

## Audit trail

Soft consumes lex-vcs tier-2 directly. Each agent decision lands as an Operation:

| Op kind (Soft-specific) | Payload |
|---|---|
| `SpawnAgent` | AgentSig, initial state, capability, peer resolutions |
| `ReceiveMessage` | envelope, payload hash |
| `Decision` | parent message id, new state hash, emitted actions |
| `GoalViolation` | rejected action, spec id, counterexample |
| `CapabilityViolation` | rejected action, missing effect/peer |
| `StopAgent` | reason, final state hash |

These are added as new variants on the Operation enum from #129; they share the same content-addressed identity, the same causal parent chain, and the same predicate-based query surface from #133.

The framework gives no special treatment to its own ops in the log — they're just operations. `lex op log --predicate '{"agent_id": "X"}'` reads them; `lex blame` walks them; tier-2 attestations (#132) attach to them. This means you get, for free:

- "Show me everything agent X has decided in the last hour."
- "Which agents have ever violated goal G?"
- "What was agent X's state at the moment it sent message M?"
- "Replay agent X from spawn through decision N." (via `lex replay` over the filtered op log)

## Library shape (Lex stdlib sketch)

Illustrative, not validated against the parser. The exact syntax may shift; the *structure* is what to fix in the doc.

```lex
import "soft.agent" as agent
import "soft.peer" as peer
import "soft.action" as action

# Declare types ----------------------------------------------------
type ConsumerState = {
  capacity :: Int,
  in_flight :: Int,
}

type Announce       = { count :: Int, deadline :: Time }
type RequestBatch   = { max :: Int }
type Batch          = { items :: List[Item] }

# Handlers (pure functions) ----------------------------------------
fn on_announce(
    s :: ConsumerState,
    msg :: Announce,
    from :: peer.Ref,
) -> [emit] (ConsumerState, List[action.Action]) {
  let available := s.capacity - s.in_flight
  if available > 0 {
    let req := action.send(from, RequestBatch({ max: available }))
    let s2  := { s | in_flight: s.in_flight + available }
    (s2, [req])
  } else {
    (s, [])
  }
}

fn on_batch(
    s :: ConsumerState,
    msg :: Batch,
    from :: peer.Ref,
) -> [emit] (ConsumerState, List[action.Action]) {
  let n := list.length(msg.items)
  let s2 := { s | in_flight: s.in_flight - n }
  (s2, [action.persist(s2)])
}

# Configuration ----------------------------------------------------
fn config() -> agent.Config {
  agent.new("consumer")
    |> agent.with_state(ConsumerState({ capacity: 10, in_flight: 0 }))
    |> agent.peers([peer.named("producer")])
    |> agent.handle("Announce", on_announce)
    |> agent.handle("Batch", on_batch)
    |> agent.goal("specs/no_overflow.spec")
    |> agent.goal("specs/processes_within_deadline.spec")
    |> agent.effects([net, time])
}
```

Salient points:

- Handlers are pure Lex functions. The `[emit]` effect is the soft framework's vocabulary for "may produce Actions"; it doesn't perform side effects directly.
- The runtime, not the agent, executes Actions. This is what makes the goal gate possible — the runtime sees the proposed Action before it happens.
- `agent.Config` is a builder. Type-check fails if a declared handler's signature doesn't match the message type registered under that name.
- Goals are referenced as `spec` files and resolved at runtime startup against the spec checker.

## Worked example: producer/consumer

The smallest example that exercises the full stack.

### Agents

- **Producer.** State: a queue of items with deadlines. Handles internal "tick" events and `RequestBatch` from the consumer. Emits `Announce` to the consumer when its queue exceeds a threshold or when items approach their deadlines.
- **Consumer.** State: capacity, in-flight count. Handles `Announce` (pulls a batch sized to its capacity) and `Batch` (processes, decrements in-flight). Goals: never exceed capacity; process every received item before its deadline.

### Soft-integration shape

The producer never *pushes* batches. It announces. The consumer never *polls*. It listens for announces and pulls when it can. Communication happens only when:

- The producer's queue grows past a threshold (announce).
- The consumer has spare capacity and an outstanding announce (request batch).
- The producer has a batch ready for an outstanding request (send batch).

Three messages, no central scheduler, no periodic ticks across the wire. Compare to a central system where producer reports queue depth every 5 seconds and central decides batching: that's 12× the messages per minute even when nothing changes.

### Goal proofs

`specs/no_overflow.spec`:

```
spec no_overflow {
  forall s :: ConsumerState, msg :: Batch:
    let n := list.length(msg.items)
    s.in_flight + n <= s.capacity
}
```

The spec checker proves this against `on_batch` symbolically (SMT) or by random sampling. If `on_batch` is updated in a way that breaks the spec, the agent fails its goal check at startup, before any message is delivered.

### Audit trail at a typical step

A single decision produces this op chain in the log:

```
op_id ← Decision {
  agent: consumer,
  parent_msg: Announce(...),
  new_state: ConsumerState({ capacity: 10, in_flight: 7 }),
  emitted: [
    Send(producer, RequestBatch({ max: 3 })),
  ],
  goal_checks: [
    { goal: no_overflow, result: passed },
    { goal: processes_within_deadline, result: passed },
  ],
}
```

`lex op log --filter '{"agent": "consumer"}'` walks decisions; `lex replay --from-spawn consumer` reconstructs state at any point.

## Worked example: sensor-network (sketch)

Larger demo for the second milestone. Three agent kinds:

- **Sensor agents** (N instances). Each owns one physical sensor. Goal: report any reading outside its calibrated range within 1 second.
- **Aggregator agents** (M instances, M ≪ N). Each is responsible for a subset of sensors. Goal: maintain a running summary of its subset, respond to queries with a bounded staleness.
- **Actuator agents.** Each owns one actuator. Goal: act on aggregator decisions within 100ms; never act on stale data.

### Soft-integration shape

- Sensors don't push readings periodically. They notify their aggregator only on out-of-range readings or when the aggregator queries them.
- Aggregators don't poll sensors. They query when their summary is stale enough to risk decision quality, and they listen for sensor-initiated alerts.
- Actuators don't subscribe to all aggregators. They register with the aggregators whose decisions they implement, and only those.

The interesting part isn't the topology — it's that **adding a sensor is local**. The new sensor agent registers with one aggregator; no central registry is told; no other sensors are aware. Removing one is equally local.

### Why this exercises the framework

- Three agent kinds → exercises the typed-message-shared-canonical-AST guarantee.
- N+M+K topology → exercises peer-list scoping vs. naive broadcast.
- Goal: bounded staleness → exercises spec checking on time-shaped invariants.
- Sensor-initiated alerts → exercises the no-polling shape end-to-end.

## Repository structure

Same workspace shape as `lex-lang` so contributors who know one know the other.

```
soft/
├── DESIGN.md               # this document, kept in sync
├── README.md               # quickstart pointing at examples/
├── Cargo.toml              # workspace
├── crates/
│   ├── soft-core/          # runtime: agent type, mailbox, scheduler
│   ├── soft-protocol/      # message envelope, peer ref, addressing
│   ├── soft-transport-inproc/  # synchronous, single-process; for tests
│   ├── soft-transport-http/    # production transport
│   ├── soft-audit/         # bridge to lex-vcs (depends on tier-2 #128)
│   └── soft-cli/           # `soft run` / `soft inspect` / `soft trace`
├── stdlib/
│   └── soft/               # Lex-side library; importable as `import "soft.agent"`
├── examples/
│   ├── 01-producer-consumer/
│   ├── 02-sensor-network/
│   └── README.md
└── docs/
    ├── concepts.md
    ├── agent-lifecycle.md
    ├── capability-model.md
    └── audit-trail.md
```

Workspace is Cargo-rooted with the Lex-side library committed under `stdlib/soft/`. The CLI tool registers the stdlib path on startup so examples can `import "soft.agent"` without a separate install step.

## First slice

Roughly 8 engineer-weeks. Same dependency-ordered shape as the lex-vcs tier-2 plan.

1. **Meta tracker** in the new repo. Mirrors `DESIGN.md`'s thesis and non-goals; links sub-issues.
2. **`soft-protocol`.** Message envelope, peer reference, addressing primitives. No transport yet. Pure types.
3. **`soft-core` skeleton.** Agent type, mailbox, scheduler. In-process only. No goal gate, no audit yet.
4. **In-process transport.** Synchronous, single-process. Where tests live. Lets the producer/consumer example run end-to-end without networking.
5. **First example: producer/consumer.** Full code, full tests. The README's quickstart points here.
6. **Goal gate.** Wire the spec checker as a pre-emit Action filter. Rejected actions are logged in-memory for now.
7. **Capability gate.** Per-action effect + peer check at execution time.
8. **HTTP transport.** First network-capable transport. Adds discovery via static config.
9. **`soft-audit`.** Bridge to lex-vcs operations. *Requires* lex-vcs tier-2 #129 + #131 to land first; until then, log to a local file with the same schema.
10. **`soft-cli`.** `soft run config.toml`, `soft inspect <agent>`, `soft trace <decision-id>`.
11. **Second example: sensor-network.** Three agent kinds, partial topology, demonstrates the value at a less-trivial scale.

The first six items are tractable without lex-vcs tier-2; they exercise the framework in-process. By the time #8 starts, lex-vcs #129 and #131 should be partly landed, and #9 picks them up.

## Open questions

Things this doc deliberately doesn't answer; they need a decision before implementation starts.

1. **Persistent state mechanism.** The runtime persists state via Actions, but where? `std.kv` for prototypes; what for production? Postgres-via-HTTP sidecar is the obvious shape but pushes durability concerns out of Lex's control. A native Lex storage primitive is a separate roadmap item.
2. **Time semantics under replay.** `[time]` is one of the effects an agent declares; replay needs to deliver the same time values to reproduce decisions. The cleanest design is to record time effect responses in the operation log alongside messages, but this bloats the log. Open question how aggressively to compact.
3. **Agent migration.** Moving a running agent between machines. Is the framework's responsibility, or out of scope? Migration is conceptually a stop-on-A + spawn-on-B with state hand-off via the audit log. Cleanly expressible; uncertain whether it deserves a v1 primitive.
4. **Failure semantics.** When a peer is unreachable, the framework can: drop the message, retry with backoff, fail the decision. Each is right in some context. Default needs to be chosen; per-handler override needs a syntax.
5. **Schema evolution.** When `Announce v1` becomes `Announce v2`, what's the migration path? Option A: a typed `Convert` function the runtime invokes when shapes don't match. Option B: agents only talk to peers with matching schemas, requiring lockstep upgrade. Option A is more flexible; B is more honest.
6. **Goal conflict.** Two goals can be jointly unsatisfiable for some Action. Today the framework rejects the Action and continues; should it instead transition the agent to a degraded mode? Domain-specific; framework should expose the choice, not pick for users.
7. **Naming.** `soft` is a placeholder. Real options under consideration: `fabric`, `mesh-of-intent`, `bdi-lex`, `quiet`, `sparse`. Final choice before repo creation.

## Related work

Brief, with the honest comparisons:

- **Erlang/OTP.** The classic. Soft is more opinionated (typed messages, declared goals) and bound to Lex's effect system. Erlang has decades of production hardening; Soft has none yet. If you're picking between them today, pick Erlang.
- **Akka (Scala/JVM).** Actor model with typed messages (Akka Typed). Closest spiritual relative on the typed-actor axis. No notion of goals, no first-class capability bounds. Heavy JVM footprint per agent.
- **Ray.** Distributed actor system aimed at ML workloads. Untyped messages, no goals, no effect bounds. Solves a different problem (compute scheduling).
- **AutoGen / CrewAI / LangChain agents.** LLM-orchestration; agents are short-lived request/response. Different timescale, different concerns. No persistent state, no audit log, no capability bounds.
- **MAS research (Jadex, JADE, Jason).** Formal BDI implementations from the agent-research community. Influence Soft's vocabulary (goals, intentions). Implementation-wise less production-shaped.
- **Service mesh (Istio, Linkerd).** Lower in the stack. Soft runs on top of mesh; mesh is not a competitor.

Soft's positioning: **typed actor system with effect-bounded capabilities and proof-checkable goals, persistent and long-running, deterministic up to message order, audit-first.** Each of those phrases is a load-bearing constraint and explains a non-goal.

## Decision log placeholder

This doc will accumulate decisions as the framework lands. Each decision belongs as a row here, with a date, a one-line summary, and a link to the discussion or PR.

| Date | Decision | Reference |
|---|---|---|
| 2026-05-06 | Initial draft committed | this doc |

