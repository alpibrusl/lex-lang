# Status

Full capability table for lex-lang v0.9.x.

## Production-ready

| Capability | Notes |
|---|---|
| Effect-typed sandbox | 7/7 adversarial cases blocked pre-execution; see [`bench/REPORT.md`](../bench/REPORT.md) |
| Content-addressed AST | SigId / body_hash stable across serialization; see [`docs/INVARIANTS.md`](INVARIANTS.md) |
| Typed Operation log (VCS tier-2) | `AddFunction`, `ModifyBody`, `ReplaceMatchArm`, `Merge`, `Candidate`, `Promote`, … |
| Typed transforms | `ReplaceMatchArm`, `RenameLocal`, `InlineLet`, `ExtractFunction` |
| Repair loop | `lex repair --apply` + `RepairAttempt` attestation; `RepairHint` with `suggested_transform` |
| `std.conc` actors | Message-passing concurrency; `spawn`, `ask`, `cast` |
| `std.sql` | SQLite + Postgres; prepared statements, typed rows |
| `std.crypto` | HMAC-SHA256, AES-GCM, blake3 |
| `std.redis` | Get/set/pub/sub |
| `std.http` | Client + `net.serve_fn` server |
| Multi-agent `Candidate / Promote` | Proposer races without CAS contention |
| Per-session budget gate | Cost tracked across all participating agents |
| `ProducerTrust` scoring | Rolling window of attestations |
| `lex-lsp` | VS Code language server (hover, go-to-def, inline effect display) |
| `lex-tea` web UI | Browser-based REPL and store explorer |
| ACLI compliance | Structured `--output json`, exit codes, `lex skill` surface |
| Spec checker | Randomized property check + SMT-LIB export |
| Fuzz CI | `cargo fuzz` targets gated in CI |
| Conformance harness | Cross-version SigId and OpId stability |
| Structural merge | `lex merge start / resolve / commit`; conflicts as typed JSON records |
| `lex blame --with-evidence` | Full attestation chain walk |

## Deferred

| Item | Blocker |
|---|---|
| `flow.parallel_record` | Needs row polymorphism |
| VCS tier-3 federation | Protocol design pending |
| JIT slice 5 | Depends on in-process Z3 |
| In-process Z3 | Linking + WASM constraints |
| Store-native imports | Import resolution design pending |
