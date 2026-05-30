#!/usr/bin/env bash
# Theatrical demo — two agents diverge, merge is structural.
# Usage:  bash demo.sh
#         asciinema rec demo.cast -c "bash demo.sh" --overwrite
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE/../.."
LEX="${LEX:-lex}"
STORE="$(mktemp -d)/lex-store"
mkdir -p "$STORE"

BOLD=$'\033[1m'; DIM=$'\033[2m'; CYAN=$'\033[36m'
GREEN=$'\033[32m'; RED=$'\033[31m'; YELLOW=$'\033[33m'
BLUE=$'\033[34m'; RESET=$'\033[0m'

slow() { echo "$@" | pv -qL 55; }
pause() { sleep "${1:-1.2}"; }
hr()  { printf '%s' "$DIM"; printf '─%.0s' {1..72}; printf '%s\n' "$RESET"; }
hdr() { echo; hr; echo "  ${BOLD}${CYAN}$*${RESET}"; hr; echo; }
cmd() { echo "${BOLD}${BLUE}\$${RESET}  $*"; pause 0.6; }

clear
echo
echo "  ${BOLD}lex-lang${RESET}  ·  Two agents, one function, structural merge"
echo "  ${DIM}Conflicts are typed records. Merge is an op in the audit log.${RESET}"
echo
sleep 2

# ── v0 on main ─────────────────────────────────────────────────────────
hdr "v0 — initial clamp, published to main"
slow "  A naive nested-match implementation."
slow "  Publishing emits a TypeCheck attestation automatically."
echo
pause 0.8

cmd "cat examples/agent_merge/v0_initial.lex"
cat examples/agent_merge/v0_initial.lex | grep -v '^#' | grep -v '^$'
echo
pause 0.8

cmd "lex publish --activate examples/agent_merge/v0_initial.lex --store \$STORE"
pause 0.4
"$LEX" --output json publish --store "$STORE" --activate examples/agent_merge/v0_initial.lex \
  | jq -r '"  → op: " + (.data.ops[0].kind.op) + "  |  TypeCheck: " +
            (.data.ops[0].attestations[0].kind.kind // "recorded")'
echo
pause 1.5

# ── Feature branch ─────────────────────────────────────────────────────
hdr "Agent A — feature branch: rewrites clamp to min/max form"
slow "  Different body, same signature. Same tests pass."
slow "  The merge engine will see this as ours-side of a ModifyModify."
echo
pause 0.8

cmd "lex branch create feature  &&  lex branch use feature"
"$LEX" branch create --store "$STORE" feature 2>&1 | grep -o 'created.*'
"$LEX" branch use    --store "$STORE" feature 2>&1 | grep -o 'on.*'
echo
pause 0.5

cmd "cat examples/agent_merge/v1_feature.lex"
cat examples/agent_merge/v1_feature.lex | grep -v '^#' | grep -v '^$'
echo
pause 0.8

cmd "lex publish --activate examples/agent_merge/v1_feature.lex --store \$STORE"
pause 0.4
"$LEX" --output json publish --store "$STORE" --activate examples/agent_merge/v1_feature.lex \
  | jq -r '[.data.ops[] | .kind.op] | "  → ops: " + join(", ")'
echo
pause 1.5

# ── Main diverges ──────────────────────────────────────────────────────
hdr "Agent B — main: adds lo > hi guard to clamp"
slow "  Meanwhile on main: a different change, same function."
slow "  Two branches. One function. Neither knows about the other."
echo
pause 0.8

"$LEX" branch use --store "$STORE" main 2>&1 | grep -o 'on.*'
cmd "cat examples/agent_merge/v1_main.lex"
cat examples/agent_merge/v1_main.lex | grep -v '^#' | grep -v '^$'
echo
pause 0.8

cmd "lex publish --activate examples/agent_merge/v1_main.lex --store \$STORE"
pause 0.4
"$LEX" --output json publish --store "$STORE" --activate examples/agent_merge/v1_main.lex \
  | jq -r '[.data.ops[] | .kind.op] | "  → ops: " + join(", ")'
echo
pause 1.5

# ── Merge ──────────────────────────────────────────────────────────────
hdr "Merge — conflict is a typed record, not a diff marker"
slow "  lex merge start detects a ModifyModify on clamp."
slow "  No <<<<<< HEAD. The conflict is structured JSON."
echo
pause 0.8

cmd "lex merge start --src feature --dst main --store \$STORE"
pause 0.4
start_out=$("$LEX" --output json merge start --store "$STORE" --src feature --dst main)
merge_id=$(echo "$start_out" | jq -r '.data.merge_id')
conflict_id=$(echo "$start_out" | jq -r '.data.conflicts[0].conflict_id')
echo "$start_out" | jq '{
  merge_id: .data.merge_id,
  conflict: .data.conflicts[0] | {kind: .kind, sig: (.conflict_id | .[0:16] + "…")}
}'
echo
pause 1.5

hdr "Resolve — agent picks TakeTheirs, commit lands as a typed op"
slow "  The resolution is a first-class operation."
slow "  Every step — publish, branch, merge, resolve, commit — is"
slow "  content-addressed and attested."
echo
pause 0.8

cat > "$STORE/resolutions.json" <<EOF
[{"conflict_id": "$conflict_id", "resolution": {"kind": "take_theirs"}}]
EOF

cmd "lex merge resolve \$MERGE_ID --file resolutions.json"
pause 0.4
"$LEX" --output json merge resolve --store "$STORE" "$merge_id" \
  --file "$STORE/resolutions.json" \
  | jq '{verdict: .data.verdicts[0].accepted, remaining: (.data.remaining_conflicts | length)}'
echo
pause 0.8

cmd "lex merge commit \$MERGE_ID"
pause 0.4
"$LEX" --output json merge commit --store "$STORE" "$merge_id" \
  | jq '{new_head: (.data.new_head_op | .[0:16] + "…"), branch: .data.dst_branch}'
echo
pause 1.5

# ── Evidence ───────────────────────────────────────────────────────────
hdr "Evidence trail — clamp has 3 stages, 3 attestations"
slow "  Every version of clamp is in the log."
slow "  The merge op itself is attested. The spec proof is attested."
echo
pause 0.8

cmd "lex spec check clamp.spec --source v1_feature.lex --trials 200"
pause 0.4
"$LEX" spec check examples/agent_merge/clamp.spec \
  --source examples/agent_merge/v1_feature.lex \
  --store  "$STORE" \
  --trials 200 \
  | jq '{status: .status, method: .evidence.method, trials: .evidence.trials}'
echo
pause 0.8

cmd "lex blame clamp --with-evidence --store \$STORE"
pause 0.4
"$LEX" --output json blame --with-evidence --store "$STORE" examples/agent_merge/v1_feature.lex \
  | jq '.data.blame[] | select(.name=="clamp") |
      {name, stages: (.history | length),
       attestations: ([.history[].attestations | length] | add)}'
echo
pause 1.5

# ── Summary ────────────────────────────────────────────────────────────
hr
echo
echo "  ${BOLD}${GREEN}DONE${RESET}"
echo
echo "  Two agents modified the same function independently."
echo "  The conflict was a typed record — resolved programmatically."
echo "  Every step is in the audit log: publish → branch → merge → spec."
echo "  Nothing was lost. Nothing was guessed."
echo
hr
echo
