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

# The advertised MSRV must match the enforced one. `rust-version` in
# Cargo.toml is what cargo actually gates on; the badge and the install
# section are what a reader believes. They drifted apart before (the README
# claimed 1.80 while the tree needed 1.95), so tie them together.
msrv="$(grep -m1 '^rust-version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
if [ -z "$msrv" ]; then
  echo "no rust-version in Cargo.toml — the README's MSRV claim would be unenforced"
  status=1
else
  claimed="$(grep -oE 'Rust [0-9]+\.[0-9]+\+|rust-[0-9]+\.[0-9]+%2B' README.md \
             | grep -oE '[0-9]+\.[0-9]+' | sort -u || true)"
  if [ -z "$claimed" ]; then
    echo "README.md states no MSRV — expected 'Rust $msrv+'"
    status=1
  fi
  while read -r c; do
    [ -z "$c" ] && continue
    if [ "$c" != "$msrv" ]; then
      echo "README.md advertises Rust $c but Cargo.toml rust-version is $msrv"
      status=1
    fi
  done <<< "$claimed"
  [ "$status" -eq 0 ] && echo "README MSRV matches Cargo.toml rust-version ($msrv)"
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
