#!/usr/bin/env bash
# Fail if .cli/ (README.md, commands.json, examples/*.sh) is stale relative to
# the `lex` binary's actual command surface.
#
# The `acli` crate rewrites .cli/ from the live clap command tree every time
# `lex` runs from the repo root, but nothing enforced that anyone actually ran
# it after adding or renaming a command (#1036: `lex files` and
# `lex publish --no-files` landed in #1007 PR 4 without a synced .cli/, and it
# went unnoticed because CI never checked). Make the drift a red build with
# the fix in the message, mirroring doc-sync --check for docs/AGENT.md.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo run -q -p lex-cli -- --help >/dev/null

if git diff --quiet --exit-code -- .cli/; then
  echo ".cli/ is in sync with the lex-cli command tree"
else
  echo ".cli/ is stale — the lex-cli command tree has changed but .cli/ wasn't regenerated:"
  echo
  git diff --stat -- .cli/
  echo
  echo "Fix: run 'cargo run -p lex-cli -- --help' locally and commit the .cli/ diff."
  exit 1
fi
