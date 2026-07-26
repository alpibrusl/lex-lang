# lex

Version: 0.10.7
ACLI version: 0.1.0

## Commands

### parse

print canonical AST as JSON

Idempotent: true

### check

type-check; exit 0 or print errors

Idempotent: true

### run

execute fn under capability policy (args parsed as JSON)

Idempotent: false

### hash

print canonical SigId/StageId hashes for each function

Idempotent: true

### blame

show each fn's stage history from the store

Idempotent: true

### publish

publish each stage in a file to the store as Draft

Idempotent: false

### store

browse the content-addressed code store

### stage

print stage info, or list attestations for a stage

Idempotent: true

### attest

cross-stage attestation queries (CI / dashboards)

### trace

print a saved execution trace tree as JSON

Idempotent: true

### replay

re-execute with effect overrides keyed by NodeId

Idempotent: false

### diff

first NodeId where two execution traces diverge

Idempotent: true

### serve

start the agent API HTTP server

Idempotent: false

### conformance

run all JSON test descriptors under a directory

Idempotent: true

### spec

Spec proof checker (randomized + SMT-LIB export)

### agent-tool

have an LLM emit a Lex tool body, run it under declared effects

Idempotent: false

### tool-registry

runtime tool registration over HTTP

### audit

structural code search by effect / call / hostname / AST kind

Idempotent: true

### ast-diff

AST-native diff: added/removed/renamed/modified fns + body patches

Idempotent: true

### ast-merge

three-way structural merge with structured JSON conflicts

Idempotent: false

### branch

snapshot branches in lex-store (tier-1 agent-native VC)

### store-merge

three-way merge between two branches in the store

Idempotent: false

### merge

stateful agent-driven merge (CLI mirror of /v1/merge/*)

### log

show the merge journal of a branch (top-level alias for `lex branch log`)

Idempotent: true

### repl

interactive evaluator (Lex source line-by-line)

Idempotent: false

### watch

re-run check or run on file save (agent inner loop)

Idempotent: false

### agent-guidelines

emit the AI-agent authoring contract (idiom rules) for this Lex toolchain

Idempotent: true

### init

scaffold a new project (lex.toml, src/, tests/, CI)

Idempotent: false

### pkg

package manager: init, add, install, list deps; publish/verify signed capability contracts

Idempotent: false

### fmt

format .lex files; --check exits 1 if any need it

Idempotent: false

### ci

run the full pipeline: pkg install, check --strict, fmt --check, test

Idempotent: false

### test

run tests/test_*.lex files (calls run_all in each)

Idempotent: true

### keygen

print a fresh Ed25519 keypair (hex) for signing stages

Idempotent: false

### canonical

encode/decode the canonical wire form of an AST

Idempotent: true

### docs

emit machine-readable API/workspace docs

Idempotent: true

### op

inspect and sync the operation log (show|log|push|pull|repack|gc)

Idempotent: false

### plan

list repair-candidate paths for a goal, cheapest-first, within budget

Idempotent: true

### repair

apply a typed repair transform to a failed op; emits a RepairAttempt

Idempotent: false

### policy

manage store policy.json (producer blocks + required attestations)

Idempotent: false

### producer-trust

recompute per-tool trust from attestation history, or export a trusted-keys keyring above a score threshold

Idempotent: false

