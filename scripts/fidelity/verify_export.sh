#!/usr/bin/env bash
# #1007 §7 fidelity verification: publish -> push/pull through a real
# in-process lex-api hub -> export-git -> compare the result against the
# source, byte-for-byte and AST-for-AST.
#
#   scripts/fidelity/verify_export.sh <local-git-repo-or-github-url> <ref>
#
# The design sketches this as a shell script driving `lex serve` + curl.
# This repo already has an established, more capable pattern for exactly
# this shape of check: `crates/lex-cli/tests/op_push_pull_files_1007.rs`
# and `op_push_lock_sync_1031.rs` spin up the real `lex-api` handler
# in-process (`lex_api::handlers::State` + `tiny_http`) and drive the real
# `lex` binary via `Command`, rather than backgrounding/killing a `lex
# serve` process and text-scraping curl output. The actual verification
# logic -- including the mandatory synthetic repo covering every row of
# the design's §2 ownership table, and the deliberate-corruption check
# that proves the assertions aren't a rubber stamp -- lives in
# `crates/lex-cli/tests/fidelity_export_1007.rs`. This script is the
# runnable-by-humans-and-CI entry point the design's §7 asks for: it
# invokes that test with `cargo test`.
#
# With no arguments, runs the mandatory, must-pass synthetic-repo check
# (no network needed -- this is what CI runs on every PR via the
# `cargo test --workspace` step, so running it here again is mostly for a
# human to reproduce locally). With <repo> <ref>, additionally runs the
# `#[ignore]`d real-repo check against that ref (a local path or anything
# `git clone` accepts) -- this is the "nightly on pinned real packages"
# mode the design describes; point it at lex-agent / lex-ocpi / lex-code
# (or any other Lex package) once those are reachable from wherever this
# runs.
set -euo pipefail

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

echo "== #1007 fidelity check: synthetic repo (mandatory, must pass) =="
# No name filter: the file's non-ignored tests are exactly the mandatory
# synthetic-repo check and the deliberate-corruption check; the real-repo
# check is `#[ignore]`d and only runs below, when a repo/ref is given.
cargo test -p lex-cli --test fidelity_export_1007 -- --nocapture

if [ "$#" -gt 0 ]; then
  if [ "$#" -ne 2 ]; then
    echo "usage: $0 [<local-git-repo-or-github-url> <ref>]" >&2
    exit 2
  fi
  echo
  echo "== #1007 fidelity check: real repo $1 @ $2 (bonus, network may be required) =="
  FIDELITY_REPO="$1" FIDELITY_REF="$2" \
    cargo test -p lex-cli --test fidelity_export_1007 fidelity_check_against_a_real_repo -- --ignored --nocapture
fi
