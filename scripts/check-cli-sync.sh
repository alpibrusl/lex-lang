#!/usr/bin/env bash
# Fail if .cli/ (README.md, commands.json, examples/*.sh) is stale relative to
# the `lex` binary's actual command surface.
#
# The `acli` crate rewrites .cli/ from the live command tree, but only on the
# `introspect` and `version` subcommands, and only when run from the repo
# root. Nothing enforced that anyone ran it after adding or renaming a command
# (#1036: `lex files` and `lex publish --no-files` landed in #1007 PR 4 without
# a synced .cli/ and drifted silently). Make that drift a red build with the
# fix in the message, mirroring doc-sync --check for docs/AGENT.md.
#
# The version stamp is ignored: releases bump it without regenerating .cli/,
# and a version-only diff says nothing about the command surface.
set -euo pipefail

cd "$(dirname "$0")/.."

cargo run -q -p lex-cli -- version >/dev/null

stale="$(git diff -U0 -- .cli/ | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' \
         | grep -vE '^[+-](Version: |  "version": )' || true)"
untracked="$(git ls-files --others --exclude-standard -- .cli/)"

if [ -z "$stale" ] && [ -z "$untracked" ]; then
  echo ".cli/ is in sync with the lex-cli command tree"
else
  echo ".cli/ is stale — the lex-cli command tree changed but .cli/ wasn't regenerated:"
  echo
  git status --short -- .cli/
  echo
  echo "Fix: run 'cargo run -p lex-cli -- version' from the repo root and commit the .cli/ diff."
  exit 1
fi
