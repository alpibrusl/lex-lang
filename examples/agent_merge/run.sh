#!/usr/bin/env bash
# End-to-end demo: agent-native VCS workflow against a fresh store.
#
# What this script shows:
#   1. Publish v0 → main. The store's write-time gate (#130) emits
#      a TypeCheck attestation automatically (#132).
#   2. Branch into `feature`, publish a divergent body there.
#   3. Diverge `main` again — now both branches have modified
#      `clamp` differently, producing a ModifyModify conflict.
#   4. Open a stateful merge session (#134), inspect the conflict.
#   5. Resolve it (this demo: TakeTheirs; an agent harness would
#      decide programmatically).
#   6. Commit. The merge op lands as a real op in the log.
#   7. Verify the spec against the merged body and persist a Spec
#      attestation (#132).
#   8. Read the evidence trail back via blame/stage/attest filter.
#
# Run from the repo root with `lex` already built (or on $PATH):
#   bash examples/agent_merge/run.sh

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STORE="${STORE:-$(mktemp -d)/lex-store}"
mkdir -p "$STORE"

LEX="${LEX:-lex}"
say() { printf '\n\x1b[1;36m▶ %s\x1b[0m\n' "$*"; }

# ----------------------------------------------------------------
say "1. publish v0 to main; TypeCheck attestation lands automatically"
"$LEX" --output json publish --store "$STORE" --activate "$HERE/v0_initial.lex" \
  | jq '{ops: .data.ops | map({kind: .kind.op}), head_op: .data.head_op}'

stage_id=$("$LEX" --output json blame --store "$STORE" "$HERE/v0_initial.lex" \
  | jq -r '.data.blame[] | select(.name=="clamp") | .here_stage_id')
echo "stage_id of clamp on main: $stage_id"

say "1b. read the evidence already on disk (TypeCheck::Passed from #147)"
"$LEX" --output json stage --store "$STORE" --attestations "$stage_id" \
  | jq '.data.attestations | map({kind: .kind.kind, result: .result.result, by: .produced_by.tool})'

# ----------------------------------------------------------------
say "2. fork: create 'feature' from main, switch, publish v1_feature"
"$LEX" branch create --store "$STORE" feature
"$LEX" branch use    --store "$STORE" feature
"$LEX" --output json publish --store "$STORE" --activate "$HERE/v1_feature.lex" \
  | jq '.data.ops | map({kind: .kind.op})'

# ----------------------------------------------------------------
say "3. switch back to main, publish v1_main — both sides have now modified clamp"
"$LEX" branch use    --store "$STORE" main
"$LEX" --output json publish --store "$STORE" --activate "$HERE/v1_main.lex" \
  | jq '.data.ops | map({kind: .kind.op})'

# ----------------------------------------------------------------
say "4. open the merge session — stateful, agent-driven (#134)"
start_out=$("$LEX" --output json merge start --store "$STORE" --src feature --dst main)
echo "$start_out" | jq '{merge_id: .data.merge_id, conflicts: .data.conflicts | map({sig: .conflict_id, kind: .kind})}'
merge_id=$(echo "$start_out" | jq -r '.data.merge_id')
conflict_id=$(echo "$start_out" | jq -r '.data.conflicts[0].conflict_id')

say "5. agent decides: take the feature branch's resolution (TakeTheirs)"
cat > "$STORE/resolutions.json" <<EOF
[{"conflict_id": "$conflict_id", "resolution": {"kind": "take_theirs"}}]
EOF
"$LEX" --output json merge resolve --store "$STORE" "$merge_id" --file "$STORE/resolutions.json" \
  | jq '{verdicts: .data.verdicts | map({sig: .conflict_id, accepted: .accepted}),
         remaining: .data.remaining_conflicts | length}'

# ----------------------------------------------------------------
say "6. commit — Merge op lands; a new TypeCheck attestation lands too"
commit_out=$("$LEX" --output json merge commit --store "$STORE" "$merge_id")
echo "$commit_out" | jq '{new_head: .data.new_head_op, dst: .data.dst_branch}'

# ----------------------------------------------------------------
say "7. verify the spec against the merged body and persist evidence (#132)"
# After the merge the active stage on main is feature's body
# (`min2`/`max2` form). The spec checker proves the contract
# against that body and writes a Spec attestation.
"$LEX" spec check "$HERE/clamp.spec" \
  --source "$HERE/v1_feature.lex" \
  --store  "$STORE" \
  --trials 200 \
  | jq '{status: .status, method: .evidence.method, trials: .evidence.trials}' \
  || echo "(spec check exits non-zero on counterexample/inconclusive — that's a verdict, not a bug)"

# ----------------------------------------------------------------
say "8. read the full evidence trail across the store"
echo "→ blame --with-evidence (per-stage history + every attestation):"
"$LEX" --output json blame --with-evidence --store "$STORE" "$HERE/v1_feature.lex" \
  | jq '.data.blame[] | {name, attestations_per_stage: .history | map(.attestations | length)}'

echo
echo "→ attest filter (cross-stage; passed Spec attestations only):"
"$LEX" --output json attest filter --store "$STORE" --kind spec --result passed \
  | jq '.data | {count, attestations: .attestations | map({kind: .kind.kind, by: .produced_by.tool, stage: .stage_id})}'

echo
say "done. store at: $STORE"
