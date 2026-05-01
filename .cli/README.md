# lex

Version: 0.1.0
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

### publish

publish each stage in a file to the store as Draft

Idempotent: false

### store

browse the content-addressed code store

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

