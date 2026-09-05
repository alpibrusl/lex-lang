#!/usr/bin/env bash
# Fail if a source file under crates/*/src exceeds the line budget (#779).
#
# Size by itself is not a defect; a VM dispatch loop is legitimately
# large. The cost of very large files is duplicated logic that nobody
# notices because the file is too big to hold in one view (#772 had
# six store-root resolvers, #774 two `list.cons` arms). This check
# keeps that pressure visible: a file over the budget is a red build
# unless it is listed in the allow-list below, and the allow-list is
# meant to shrink, never grow.
set -euo pipefail

cd "$(dirname "$0")/.."

budget=2500

# Files known to exceed the budget, with the issue tracking their split.
# Remove an entry once the file is under budget; add one only with an
# issue reference.
allow=(
  "crates/lex-runtime/src/builtins.rs"    # #778
  "crates/lex-store/src/store.rs"         # #779
  "crates/lex-types/src/builtins.rs"      # #778
)

is_allowed() {
  local f="$1"
  for a in "${allow[@]}"; do
    [ "$a" = "$f" ] && return 0
  done
  return 1
}

status=0
checked=0
for a in "${allow[@]}"; do
  if [ ! -f "$a" ]; then
    echo "$a is allow-listed but does not exist; remove it from the allow-list in $0"
    status=1
  fi
done
while IFS= read -r f; do
  checked=$((checked + 1))
  n="$(wc -l < "$f")"
  if [ "$n" -gt "$budget" ]; then
    if is_allowed "$f"; then
      continue
    fi
    echo "$f has $n lines (budget $budget); split it along its seams (see #779) or add it to the allow-list in $0 with an issue reference"
    status=1
  elif is_allowed "$f"; then
    echo "$f is under budget ($n lines); remove it from the allow-list in $0"
    status=1
  fi
done < <(find crates -path '*/src/*' -name '*.rs' -not -path '*/target/*' | sort)

[ "$checked" -gt 0 ] || { echo "no source files found — check the find pattern in $0"; exit 1; }

if [ "$status" -eq 0 ]; then
  echo "all $checked source files within the $budget-line budget (or allow-listed)"
fi
exit "$status"
