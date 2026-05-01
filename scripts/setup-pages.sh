#!/usr/bin/env bash
# One-shot repo setup: flip default branch to main, enable GitHub Pages
# from /docs, and seed description + topics. Run once after merging
# the landing-page PR.
#
# Requirements:
#   - GitHub CLI (gh) installed: https://cli.github.com/
#   - Authenticated: `gh auth status` shows you're logged in
#   - Push permission on the repo
#
# Usage:
#   bash scripts/setup-pages.sh
#
# Idempotent — safe to re-run; subsequent invocations are no-ops.

set -euo pipefail

REPO="alpibrusl/lex-lang"
BRANCH="main"
PAGES_PATH="/docs"
DESCRIPTION="Sandbox for agent-written code, via static effect typing — type-check rejects malicious LLM-generated bodies before they run."
TOPICS=(programming-language effect-system sandbox llm agent-tools claude rust)

# --- preflight -------------------------------------------------------

command -v gh >/dev/null || { echo "error: gh CLI not installed (https://cli.github.com/)"; exit 1; }
gh auth status >/dev/null 2>&1 || { echo "error: gh not authenticated (run: gh auth login)"; exit 1; }
gh repo view "$REPO" >/dev/null 2>&1 || { echo "error: can't access $REPO (typo or no permission?)"; exit 1; }

echo "→ target repo: $REPO"

# --- 1. default branch -----------------------------------------------

current_default=$(gh repo view "$REPO" --json defaultBranchRef -q '.defaultBranchRef.name')
if [[ "$current_default" != "$BRANCH" ]]; then
  echo "→ flipping default branch: $current_default → $BRANCH"
  gh repo edit "$REPO" --default-branch "$BRANCH"
else
  echo "✓ default branch already $BRANCH"
fi

# --- 2. description + topics -----------------------------------------

current_desc=$(gh repo view "$REPO" --json description -q '.description // ""')
if [[ "$current_desc" != "$DESCRIPTION" ]]; then
  echo "→ updating description"
  gh repo edit "$REPO" --description "$DESCRIPTION"
else
  echo "✓ description already set"
fi

for t in "${TOPICS[@]}"; do
  gh repo edit "$REPO" --add-topic "$t" >/dev/null 2>&1 || true
done
echo "✓ topics seeded: ${TOPICS[*]}"

# --- 3. enable GitHub Pages ------------------------------------------

# `gh api` POST is idempotent-ish: returns 409 if Pages already exists,
# in which case we PATCH the existing config instead.
pages_status=$(gh api "repos/$REPO/pages" --silent 2>/dev/null && echo present || echo absent)
if [[ "$pages_status" == "absent" ]]; then
  echo "→ enabling Pages from $BRANCH$PAGES_PATH"
  gh api -X POST "repos/$REPO/pages" \
    -f "source[branch]=$BRANCH" \
    -f "source[path]=$PAGES_PATH" >/dev/null
else
  echo "→ updating Pages source to $BRANCH$PAGES_PATH"
  gh api -X PUT "repos/$REPO/pages" \
    -f "source[branch]=$BRANCH" \
    -f "source[path]=$PAGES_PATH" >/dev/null
fi

# --- 4. report -------------------------------------------------------

owner=$(echo "$REPO" | cut -d/ -f1)
name=$(echo "$REPO" | cut -d/ -f2)
url="https://${owner}.github.io/${name}/"

echo
echo "Done. The first Pages build takes 30–60s."
echo "Landing page will be live at: $url"
echo
echo "Verify with:"
echo "  gh api repos/$REPO/pages --jq '.html_url, .status'"
