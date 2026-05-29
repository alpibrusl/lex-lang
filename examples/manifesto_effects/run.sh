#!/usr/bin/env bash
# Manifesto demo (lex-lang): effects are machine-verifiable constraints.
#
# Validates §IV's claim "the type system tells the caller" by asserting:
#   1. honest.lex type-checks      — truthful effect rows are accepted.
#   2. dishonest.lex is REJECTED   — a [net] call under an [io] signature
#                                    cannot compile.
# Exit 0 only if BOTH hold, so running this script *is* the proof.
#
#   bash examples/manifesto_effects/run.sh
#   LEX=/path/to/lex bash examples/manifesto_effects/run.sh   # custom binary
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LEX="${LEX:-lex}"
fail=0

echo "==> honest.lex must type-check (effect rows tell the truth)"
if "$LEX" check "$HERE/honest.lex" >/dev/null 2>&1; then
  echo "    OK: honest.lex type-checks"
else
  echo "    FAIL: honest.lex should type-check but did not:"
  "$LEX" check "$HERE/honest.lex" 2>&1 | head -8
  fail=1
fi

echo "==> dishonest.lex must be REJECTED (claims [io], body touches [net])"
if "$LEX" check "$HERE/dishonest.lex" >/dev/null 2>&1; then
  echo "    FAIL: dishonest.lex type-checked but should have been rejected"
  fail=1
else
  echo "    OK: the checker refused the mislabeled effect row. Reason:"
  reason="$("$LEX" --output json check "$HERE/dishonest.lex" 2>&1)"
  echo "$reason" | head -20 || true
  if ! grep -qi '"effect": *"net"\|effect-not-declared' <<<"$reason"; then
    echo "    FAIL: rejection was not the expected effect-not-declared(net) error"
    fail=1
  fi
fi

if [ "$fail" -eq 0 ]; then
  echo "VALIDATED: effects are machine-verifiable — the type IS the contract."
else
  echo "FAILED: see errors above."
fi
exit "$fail"
