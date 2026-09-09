#!/usr/bin/env bash
# Fail if crates-io.yml publishes crates in an order the dependency graph
# does not allow.
#
# `cargo publish` resolves each crate's dependencies from the registry, so a
# crate must be published after everything it depends on. That order was
# maintained by hand in the workflow and was wrong: `lex-runtime` sat fifth
# while depending on `lex-store`, which is seventh. Nothing caught it,
# because the failure only appears when a *new* version is tagged — an
# already-published version is skipped by the idempotency check, so re-runs
# of an old release looked fine.
#
# It cost v0.10.13, v0.10.16 and v0.10.17, which never reached crates.io at
# all, and stopped v0.11.0 after four of ten crates.
#
# Ordering is derived here rather than restated, so the list cannot drift
# from the graph again.
set -euo pipefail

cd "$(dirname "$0")/.."

WORKFLOW=".github/workflows/crates-io.yml"
[ -f "$WORKFLOW" ] || { echo "missing $WORKFLOW"; exit 1; }

# The list the workflow actually iterates, between `for crate in` and `do`.
mapfile -t declared < <(
  awk '/for crate in/{f=1;next} f&&/^ *do$/{exit} f{gsub(/[\\ ]/,"");if($0!="")print}' "$WORKFLOW"
)
[ "${#declared[@]}" -gt 0 ] || { echo "could not read the crate list from $WORKFLOW"; exit 1; }

printf '%s\n' "${declared[@]}" | python3 -c '
import json, subprocess, sys

declared = [l.strip() for l in sys.stdin if l.strip()]
md = json.loads(subprocess.run(
    ["cargo", "metadata", "--format-version", "1", "--no-deps"],
    capture_output=True, text=True, check=True).stdout)

names = set(declared)
# Only normal/build dependencies constrain publish order. A dev-dependency
# may point back up the graph; cargo strips one that carries no `version`,
# which is how the lex-trace -> lex-runtime cycle is broken.
deps = {}
for p in md["packages"]:
    if p["name"] not in names:
        continue
    deps[p["name"]] = {
        d["name"] for d in p["dependencies"]
        if d["name"] in names and d["name"] != p["name"] and d["kind"] != "dev"
    }

pos = {c: i for i, c in enumerate(declared)}
bad = [(c, d) for c in declared for d in sorted(deps.get(c, ()))
       if pos[d] > pos[c]]

if bad:
    print("crates-io.yml publishes crates before their dependencies:\n")
    for c, d in bad:
        print(f"  {c} (position {pos[c]+1}) needs {d} (position {pos[d]+1})")
    print("\nA correct order is:\n")
    order, seen = [], set()
    def visit(n, stack=()):
        if n in seen:
            return
        if n in stack:
            sys.exit(f"\ncycle in normal dependencies at {n} — not fixable by reordering")
        for d in sorted(deps.get(n, ())):
            visit(d, stack + (n,))
        seen.add(n); order.append(n)
    for n in declared:
        visit(n)
    for n in order:
        print(f"  {n}")
    sys.exit(1)

# A dev-dependency carrying a version can still deadlock the publish even
# when the normal-dependency order is fine, so check that separately.
dev_versioned = []
for p in md["packages"]:
    if p["name"] not in names:
        continue
    for d in p["dependencies"]:
        if (d["kind"] == "dev" and d["name"] in names
                and d["name"] != p["name"] and d.get("req") not in (None, "*")
                and pos[d["name"]] > pos[p["name"]]):
            dev_versioned.append((p["name"], d["name"], d["req"]))

if dev_versioned:
    print("a versioned dev-dependency points at a crate published later:\n")
    for c, d, req in dev_versioned:
        print(f"  {c} dev-depends on {d} {req}, published at position {pos[d]+1}")
    print("\nDrop the `version` so cargo strips it from the published manifest.")
    sys.exit(1)

print(f"publish order is consistent with the dependency graph ({len(declared)} crates)")
'
