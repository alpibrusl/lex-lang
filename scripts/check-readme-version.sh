#!/usr/bin/env bash
# Fail if README.md advertises an install version other than the workspace's.
#
# The install snippets (release tarball, container tags) are copy-pasted by
# newcomers, so a stale version there hands them an old binary. This check
# makes the drift a CI failure instead of a silent papercut.
set -euo pipefail

cd "$(dirname "$0")/.."

want="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
[ -n "$want" ] || { echo "could not read version from Cargo.toml"; exit 1; }

# Every `lex-vX.Y.Z-` tarball name and `ghcr.io/alpibrusl/lex:vX.Y.Z` tag.
found="$(grep -oE 'lex-v[0-9]+\.[0-9]+\.[0-9]+-|ghcr\.io/alpibrusl/lex:v[0-9]+\.[0-9]+\.[0-9]+' README.md \
         | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sort -u || true)"

[ -n "$found" ] || { echo "no version references found in README.md — check the patterns in $0"; exit 1; }

status=0
while read -r v; do
  if [ "$v" != "$want" ]; then
    echo "README.md advertises v$v but Cargo.toml is $want"
    status=1
  fi
done <<< "$found"

if [ "$status" -eq 0 ]; then
  echo "README install version matches Cargo.toml ($want)"
else
  echo
  echo "Fix: update the install snippets in README.md to v$want."
fi

# No hardcoded dependency pins. A downstream package's version belongs on the
# registry, which is the only place that can't go stale — a number copied into
# this README drifts the moment that package releases, and nothing here can
# detect it. The `lex` install snippets above are the exception: they're
# checked against Cargo.toml, so they're allowed to carry a version.
pins="$(grep -nE 'version[[:space:]]*=[[:space:]]*"[0-9]+\.[0-9]+' README.md || true)"
if [ -n "$pins" ]; then
  echo
  echo "README.md pins a dependency version:"
  echo "$pins"
  echo
  echo "Fix: drop the number and point at https://hub.lexlang.org instead."
  status=1
fi

exit "$status"
