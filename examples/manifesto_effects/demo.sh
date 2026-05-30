#!/usr/bin/env bash
# Theatrical demo — effects are the contract.
# Usage:  bash demo.sh
#         asciinema rec demo.cast -c "bash demo.sh" --overwrite
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE/../.."
LEX="${LEX:-lex}"

BOLD=$'\033[1m'; DIM=$'\033[2m'; CYAN=$'\033[36m'
GREEN=$'\033[32m'; RED=$'\033[31m'; BLUE=$'\033[34m'; RESET=$'\033[0m'

slow() { echo "$@" | pv -qL 55; }
pause() { sleep "${1:-1.2}"; }
hr()  { printf '%s' "$DIM"; printf '─%.0s' {1..72}; printf '%s\n' "$RESET"; }
hdr() { echo; hr; echo "  ${BOLD}${CYAN}$*${RESET}"; hr; echo; }
cmd() { echo "${BOLD}${BLUE}\$${RESET}  $*"; pause 0.6; }

clear
echo
echo "  ${BOLD}lex-lang${RESET}  ·  Effects are the contract"
echo "  ${DIM}The type system is the sandbox.${RESET}"
echo
sleep 2

# ── Honest ─────────────────────────────────────────────────────────────
hdr "honest.lex — effect row tells the truth"
slow "  fn fetch declares [net]. Any caller — human or agent —"
slow "  knows it touches the network without reading the body."
echo
pause 1

cmd "grep 'fn fetch\|fn double' examples/manifesto_effects/honest.lex"
grep 'fn fetch\|fn double' examples/manifesto_effects/honest.lex
echo
pause 0.8

cmd "lex check examples/manifesto_effects/honest.lex"
pause 0.4
"$LEX" check examples/manifesto_effects/honest.lex
echo "${GREEN}${BOLD}✓  ok${RESET}  — truthful rows accepted"
echo
pause 2

# ── Dishonest ──────────────────────────────────────────────────────────
hdr "dishonest.lex — same body, lying signature  →  REJECTED"
slow "  Same network call. Signature claims [io] — local I/O only."
slow "  The type checker cannot be convinced."
echo
pause 1

cmd "grep 'fn fetch' examples/manifesto_effects/dishonest.lex"
grep 'fn fetch' examples/manifesto_effects/dishonest.lex
echo
pause 0.8

cmd "lex check examples/manifesto_effects/dishonest.lex"
pause 0.4
"$LEX" --output json check examples/manifesto_effects/dishonest.lex 2>&1 \
  | python3 -c "
import json, sys
d = json.load(sys.stdin)
for e in d['data']['errors']:
    pos = e['position']
    eff = e.get('effect', '?')
    print(f\"  error[{e['kind']}]  effect '{eff}' not declared\")
    print(f\"    --> examples/manifesto_effects/dishonest.lex:{pos['line']}\")
    print()
"
echo "${RED}${BOLD}✗  REJECTED${RESET}  — [io] signature is a guarantee, not a comment."
echo "  ${DIM}A net call in an [io] body is a type error, not a warning.${RESET}"
echo
pause 2

# ── Summary ────────────────────────────────────────────────────────────
hr
echo
echo "  ${BOLD}${GREEN}VALIDATED${RESET}"
echo
echo "  The declared effect row IS the contract."
echo "  The type system enforces it before a byte runs."
echo "  Agents can't lie about what they touch."
echo
hr
echo
