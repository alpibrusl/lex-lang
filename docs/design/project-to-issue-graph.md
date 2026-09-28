# Project → issue graph: compile the graph before implementing it

**Status:** design proposal, no code yet. Filed as a narrower alternative
to `lex-loom`'s sprint-cycle orchestration (`lex-loom/docs/design/
sprint-cycles.md`), for the same "turn a goal into working code
systematically" problem, after real friction running that design. This
doc names a smaller mechanism and shows it needs almost no new code —
`lex-vcs` already has nearly everything it depends on.

## Summary

- **A project decomposes into a signature graph, not a sprint plan.**
  One typed [`Issue`](../../crates/lex-vcs/src/issue.rs) per function to
  be written; `deps` is that function's own callees, already a real
  field on `Issue`.
- **The whole graph is typechecked once, before any issue opens for
  implementation.** Every function in the project gets a real signature
  and a placeholder body up front; the candidate program — the whole
  thing, cross-references included — goes through the existing gate
  once. Interface drift between issues (a caller assuming a callee
  returns something it doesn't) is caught here, mechanically, before a
  single line of real logic is written — not discovered by an
  implementer three issues later.
- **Reuses `SigId`, `Issue`, `Acceptance::TypedDelta`, `gate.rs`'s
  `check_program`, and `lex issue verify` unmodified.** The only new
  pieces are one placeholder-body convention and a driver that wires
  them together. No new `Acceptance` shape, no new `OperationKind`, no
  changes to the gate.

## 1. The problem this is answering

`sprint-cycles.md` proposes a general orchestration layer: an Architect
agent emits a `SprintGraph` of typed phases (Design → Implementation →
QA → Demo → …), each phase gated by an attestation, four communication
planes (queue, A2A, VCS, audit trail) carrying the handoffs between
them. It's general-purpose and it's still v0.0 — no implementation, per
its own header — so this doc isn't a response to a documented
postmortem, only to the stated experience of running it and finding it
had problems.

The one thing worth naming without more evidence than that: a phase
like "Design" or "QA" is gated by a `Spec` an agent (or a human) wrote
for that node — a spec that has to be kept honest against what the
phase actually produces. A type checker never drifts from the program
it's checking, because it *is* the same artifact being read both times.
Where this proposal differs is narrower scope in exchange for a
stronger, harder-to-drift gate: it only covers "turn a set of function
signatures into implementations," and its gate is `lex check` on the
real candidate program, not a spec describing what the phase is
supposed to have done.

## 2. What already does this job — read from the source, not assumed

| Need | Existing mechanism |
|---|---|
| A per-function contract, independent of the body | `SigId = SHA256(canonical_json({name, input_types, output_type, effects[, examples]}))` — `lex-ast::sig_id`, no body involved at all |
| A unit of work with a declared, checkable "done" | `Issue { issue_id, title, body, acceptance, base, deps, project, created_at }` — content-addressed, `deps: BTreeSet<IssueId>` already models a dependency graph |
| A structured (not prose) acceptance criterion for "this function exists with this signature and behaves like these examples" | `Acceptance::TypedDelta { api: Vec<ApiEntry{name, signature, kind}>, examples: Vec<String> }` — the one `Acceptance` shape that's fully typed rather than a prose string in a typed wrapper |
| Grouping issues under a goal | `Issue.project: Option<String>` — already documented as "a project is a subgraph with a goal" |
| Checking a candidate program (not just one function in isolation) against everything it calls | `gate.rs::check_and_apply` wraps `lex_types::check_program(candidate)`, where `candidate` is the *whole* program's stage sequence after the op applies — cross-references are already checked together, not per-function |
| Checking one function's real body against its frozen signature, plus running its examples | `lex issue verify` (`lex-cli/src/issue.rs`): resolves `deps` (an unverified dep short-circuits to `Inconclusive`), runs the API-delta check, then the examples, and records an attestation either way |
| Refining acceptance after the issue exists, without changing its identity | `lex issue propose`/`approve`/`reject` + `with_effective_acceptance` — the issue's own content hash never moves; the effective acceptance is resolved at read time from the latest approved proposal |

Nothing in this table needs to change. `predicate.rs`'s "predicate
branches" are a saved query over the op log (`All`, `Intent`,
`AncestorOf`, …) — a retrieval filter over already-typechecked history,
unrelated to checking a set of not-yet-implemented signatures together
despite the name overlap with "predicate" in the everyday sense. Worth
saying so explicitly so nobody goes looking for cross-issue
type-consistency checking there.

## 3. The one real gap: there is no way to write "no body yet"

`Stage::FnDecl.body` is a plain `CExpr`, not `Option<CExpr>`
(`lex-ast/src/canonical.rs:41`), and the parser's `parse_fn_decl`
unconditionally parses a block for it (`lex-syntax/src/parser.rs:567`).
There is no forward-declaration syntax in Lex today — `fn foo(x :: Int)
-> Int;` with no body is a parse error, and no existing builtin
(checked: no `todo`/`unimplemented`/`panic`/`abort` in
`lex-types::builtins`) serves as a polymorphic "not implemented yet,
traps if reached" placeholder either.

Two ways to close this, in order of how little they touch:

1. **A new builtin, not new syntax.** `todo() -> a` — polymorphic
   return, an ordinary function call the existing parser and checker
   already handle, traps unconditionally at runtime if it's ever
   actually reached. A function's placeholder body is then just
   `{ todo() }`, legal today except for the one missing builtin. This
   is the smaller change and the one this doc recommends starting
   with — it needs its own narrow review (what row does calling it
   cost, if any; does it need a message argument for a better trap;
   should it be `pub`) but touches nothing about the mandatory-body
   invariant `lex-ast`/`lex-syntax` currently hold.
2. **A real forward-declaration form in the grammar** (`fn foo(x ::
   Int) -> Int;`), if `todo()` turns out to be awkward in practice —
   e.g. it can't state an effect row a real implementation would need
   but a placeholder call wouldn't perform, so a project whose
   functions have non-trivial rows might need the row written on the
   stub even though its body doesn't perform it yet. Bigger surface
   (parser, `Stage::FnDecl`, `check_program`'s exhaustiveness
   assumptions), not proposed as the first step.

Everything below assumes (1).

## 4. The flow

1. **Propose the graph.** Given a project's goal, produce a set of
   function signatures (name, input/output types, effect row) and the
   call edges between them — not prose descriptions of behavior, just
   the typed interface. Every function's body is `{ todo() }`. The
   whole file/module is real, parseable, typecheckable Lex source at
   this point, just with no logic in it.
2. **Compile the skeleton — the drift check.** Run `check_program`
   (the same call `gate.rs` already makes) against this whole
   placeholder-bodied candidate, once, before any `Issue` exists. This
   catches, mechanically, before implementation starts:
   - a caller assuming a callee's signature is something it isn't;
   - an effect row a caller declares that its callees' declared rows
     don't support;
   - a function the graph's own edges implied that was never declared.

   This is the "compile the issues and make sure they make sense
   before implementing" step — done once, on the whole graph, with the
   real type checker, not a spec written about the graph.
3. **Skeleton → issues, one per function.** For each stub, create one
   `Issue`:
   - `project`: the project's own id/name.
   - `deps`: the function's callees that are *also* issues in this
     project (already exactly what `deps` is for).
   - `acceptance`: `TypedDelta { api: [ApiEntry { name, signature,
     kind: Added }], examples: [...] }` — the signature is already
     fixed (it's the `SigId` the skeleton just checked); examples are
     written at issue-creation time or proposed later via `lex issue
     propose` and reviewed with `approve`/`reject`, unchanged from how
     that already works.
   - `base`: the skeleton's own op — the "before" state is "compiles,
     does nothing"; "done" is a real body replacing `{ todo() }` with
     the same `SigId` and passing examples.
4. **Implement in dependency order.** Topologically sort the issue
   graph. For each issue whose deps are already verified, one agent (or
   person) replaces that one function's `{ todo() }` with a real body
   plus examples. Issues with no edge between them are independently
   implementable in parallel. `lex issue verify` is the existing,
   unmodified gate: does the new body's real `SigId` still match what
   the issue promised, do the examples pass.

## 5. What this buys, and what it honestly doesn't

**Buys:** an implementer of an issue that calls another issue's
function never has to wonder whether that function's contract is what
they think it is — the whole graph already typechecked together before
either issue was implemented. Interface drift between issues is a
compile error at skeleton time, not an integration surprise discovered
mid-implementation or at merge time.

**Doesn't buy:** correctness of intent. Typechecking a skeleton proves
the *interfaces* are mutually consistent; it says nothing about whether
they capture the right goal. `examples {}` is what actually pins down
behavior precisely enough to check — which is why acceptance for each
issue is the signature *and* the examples together, not the signature
alone, and why step 1 (proposing the graph) is still the highest-
judgment, least-mechanical step in this whole flow. Nothing here
automates *that* part; it only makes sure everything built on top of it
stays honest once it's decided.

## 6. Compared to `lex-loom`'s sprint cycles

| | `sprint-cycles.md` | This proposal |
|---|---|---|
| Unit of work | A `Node` in a `SprintGraph`, arbitrary role/capability | One function, one `Issue` |
| What gates a transition | A `Spec` (schema) an agent wrote for that node | `check_program` on the real candidate program |
| Cross-node consistency | Checked by each node's own gate + a meta-spec (DAG shape, effect containment) | Checked once, for the whole graph, before any implementation, by the compiler itself |
| Communication | Four planes: queue, A2A, VCS artifacts, audit trail | One: the op log / `Issue` records already in `lex-vcs` |
| New infrastructure needed | `lex-jobs`, A2A wiring, `SprintGraph` schema + validator, phase executor | One builtin (`todo()`), one driver script |

Not a claim that this replaces `lex-loom` outright — a project big
enough to need Design/QA/Demo/Retro as distinct phases with different
tooling per phase may still want that machinery. This is scoped to the
one thing `lex-vcs` already does almost all of the work for: turning a
signature graph into implementations without letting the graph drift
out from under the issues built on it.

## 7. Open questions / first concrete tasks

- Land `todo() -> a` (§3, option 1) as a real builtin — the one
  genuinely new piece of language surface this depends on.
- Write the driver: given a proposed signature graph, emit the
  placeholder-bodied module, run `check_program`, and on success mint
  one `Issue` per function with `deps` derived from the call edges.
  Everything it calls already exists; this is glue, not a new
  subsystem.
- Decide whether the *first* skeleton-compile step for a project should
  itself require review (a human or agent looking at the proposed
  signature graph before any typechecking happens) — this doc's own
  §5 caveat is that nothing mechanical validates the graph captures the
  right goal, only that it's internally consistent.
- Try it on one real, small project end to end before deciding whether
  it needs anything `lex-loom`'s own machinery already has and this
  doesn't (parallelism scheduling beyond "topological order," a queue
  for many concurrent implementers, audit-trail integration beyond what
  `lex issue verify`'s attestations already give).
