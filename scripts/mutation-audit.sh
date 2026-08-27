#!/usr/bin/env bash
# Check that a package's tests can actually fail.
#
# A green suite is not evidence of a working suite. lex-lang#757 shipped a test
# runner that discarded `run_all`'s verdict, so failing suites reported `ok` for
# weeks; 0.10.12 fixed that, and a fleet-wide run of THIS script then found the
# same failure mode living one level up, where no toolchain check reaches it:
#
#   - two tests defined but never added to the list `run_all` folds over, so
#     they had never executed at all
#   - existence helpers shaped `fold(xs, false, acc or x == target)`, which
#     answer "contains anything else" just as happily as "contains the expected
#     item", so the assertions built on them established nothing
#
# Neither is visible to `lex test`. Both are visible here: change one operator,
# and require the suite to notice.
#
# Usage:
#   scripts/mutation-audit.sh [PACKAGE_DIR ...]     (default: current directory)
#
# Output, one line per package:
#   NAME|none|N            N mutants, all caught — the suite discriminates
#   NAME|<sites>|N         sites the suite did NOT notice; each needs reading
#   NAME|BASELINE-RED|0    already failing, so nothing can be concluded
#   NAME|NO-TESTS|0        no tests/ or test/ directory
#
# A survivor is a lead, not a verdict. Read each one before believing it: the
# ones found so far split into real weaknesses, dead helpers nothing calls, and
# disjunctions whose second branch no input reaches.
#
# Requires: `lex` on PATH, python3, and a git worktree (files are restored with
# `git checkout`, so uncommitted work in tests/ is REVERTED — commit first).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
sites_py="$here/mutation_sites.py"

# The audit is about whether assertions discriminate, not about what the tests
# are permitted to touch, so this grants everything rather than making each
# package's policy the auditor's problem.
effects="io,time,crypto,random,sql,fs_read,fs_write,net,concurrent,env,llm,proc,approval,stream"

audit_package() {
  local dir="$1" name
  name="$(basename "$dir")"
  cd "$dir"

  shopt -s nullglob
  local files=(tests/*.lex test/*.lex)
  shopt -u nullglob
  if [ ${#files[@]} -eq 0 ]; then
    echo "$name|NO-TESTS|0"
    return
  fi

  if ! lex test --allow-effects "$effects" tests/ >/dev/null 2>&1; then
    echo "$name|BASELINE-RED|0"
    return
  fi

  local total=0 survivors="" file base site first fast
  for file in "${files[@]}"; do
    base="$(basename "$file")"
    local site_list
    site_list="$(python3 "$sites_py" list "$file" | tr '\n' ' ')"
    [ -z "$site_list" ] && continue

    # `lex test` is the only thing that INTERPRETS a run_all verdict. Running
    # one file with `lex run <file> run_all` is far cheaper, but exits 0 for a
    # run_all that RETURNS its results instead of raising — the verdicts are a
    # value nobody reads, which is lex-lang#757 again, here in the auditor.
    # So calibrate rather than assume: break the file on purpose, and use the
    # cheap runner only if the cheap runner notices.
    first="${site_list%% *}"
    python3 "$sites_py" apply "$file" "$first"
    if lex run --allow-effects "$effects" "$file" run_all >/dev/null 2>&1; then
      fast=0
    else
      fast=1
    fi
    git checkout -q -- "$file"

    for site in $site_list; do
      total=$((total + 1))
      # An apply that fails leaves the file unmutated, and an unmutated file
      # passes — which would be scored as a survivor. Abort instead.
      if ! python3 "$sites_py" apply "$file" "$site"; then
        git checkout -q -- "$file"
        echo "$name|APPLY-FAILED:$base@$site|$total"
        return 1
      fi
      if [ "$fast" -eq 1 ]; then
        lex run --allow-effects "$effects" "$file" run_all >/dev/null 2>&1 \
          && survivors="$survivors $base@$site"
      else
        lex test --allow-effects "$effects" tests/ >/dev/null 2>&1 \
          && survivors="$survivors $base@$site"
      fi
      git checkout -q -- "$file"
    done
  done

  echo "$name|${survivors:-none}|$total"
}

status=0
for dir in "${@:-$PWD}"; do
  if [ ! -d "$dir" ]; then
    echo "no such directory: $dir" >&2
    status=1
    continue
  fi
  ( audit_package "$dir" ) || status=1
done
exit "$status"
