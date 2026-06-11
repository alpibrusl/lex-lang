#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# Supply-chain provenance, end to end.
#
# A package's required capability is DERIVED from its typed effects, signed into
# a capability contract, served from a registry, VERIFIED on install against a
# pinned publisher, promoted into a durable attestation, and turned into EARNED
# trust that gates the next install. One `lex` binary; no network.
#
#   publish (derive grant from effects, sign)
#        └─▶ install (verify signature + content hash + trusted signer)
#                 └─▶ attest (promote the install into the attestation graph)
#                          └─▶ producer-trust (earn a keyring from track record)
#
# Run:  bash demo/supply-chain.sh
# Needs: the `lex` binary (built at target/debug/lex, or set $LEX) and python3.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEX="${LEX:-$REPO_ROOT/target/debug/lex}"
[ -x "$LEX" ] || { echo "build first: cargo build -p lex-cli  (or set \$LEX)"; exit 1; }
command -v python3 >/dev/null || { echo "this demo needs python3 (the stand-in registry)"; exit 1; }

WORK="$(mktemp -d)"
STORE="$WORK/store"
trap 'rm -rf "$WORK"; [ -n "${REG_PID:-}" ] && kill "$REG_PID" 2>/dev/null || true' EXIT

say()  { printf '\n\033[1;36m=== %s\033[0m\n' "$*"; }
note() { printf '    %s\n' "$*"; }
field(){ python3 -c "import sys,json;print(json.load(sys.stdin)['data']['$1'])"; }

# ── 0. A publisher key the consumer will (learn to) trust ─────────────────────
# A fixed 32-byte seed (64 hex) so the demo is reproducible; the matching public
# key is read back from the signed contract after publishing.
SECRET="1122334455667788990011223344556677889900112233445566778899001122"

# ── 1. PUBLISH — grant DERIVED from the code's typed effects, then signed ──────
say "1. Publish — the contract's grant is derived from the package's typed effects"
mkdir -p "$WORK/pkg/src"
cat > "$WORK/pkg/lex.toml" <<EOF
[package]
name = "weather"
version = "1.0.0"
EOF
cat > "$WORK/pkg/src/main.lex" <<'EOF'
import "std.net" as net
import "std.fs" as fs
fn main() -> [net, fs_walk] Bool {
  let _ := fs.exists("/etc/hostname");
  match net.get("https://wttr.in/Paris") { Ok(_) => true, Err(_) => false }
}
EOF
note "src/main.lex declares [net, fs_walk] — the type checker propagates every callee's effects."
( cd "$WORK/pkg" && "$LEX" pkg publish --sign "$SECRET" --derive-grant \
    --contract-out "$WORK/contract.json" --archive-out "$WORK/archive.tar" --no-upload )
PUBLIC="$(python3 -c "import json;print(json.load(open('$WORK/contract.json'))['signer'])")"
note "→ least authority, inferred from the code — the publisher can't over- or under-declare."

# ── 2. REGISTRY — serve the signed contract + archive over HTTP ───────────────
say "2. A registry serves the package (a python http.server stands in)"
mkdir -p "$WORK/registry/v1/pkg/weather/1.0.0"
cp "$WORK/contract.json" "$WORK/registry/v1/pkg/weather/1.0.0/contract"
cp "$WORK/archive.tar"   "$WORK/registry/v1/pkg/weather/1.0.0/archive"
( cd "$WORK/registry" && python3 -m http.server 0 >"$WORK/reg.log" 2>&1 ) &
REG_PID=$!
sleep 0.5
PORT="$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$WORK/reg.log" | head -1)"
REG="http://127.0.0.1:$PORT"
note "registry at $REG"

# ── 3. INSTALL — verify signature + content hash + a PINNED publisher ─────────
say "3. Install — the consumer verifies the contract before trusting the dep"
printf '{"trusted":["%s"]}\n' "$PUBLIC" > "$WORK/keyring.json"
mkdir -p "$WORK/app"
cat > "$WORK/app/lex.toml" <<EOF
[package]
name = "app"
version = "0.1.0"

[dependencies]
weather = { registry = "$REG", version = "1.0.0" }
EOF
note "keyring trusts only: ${PUBLIC:0:16}…"
( cd "$WORK/app" && LEX_PACKAGES_DIR="$WORK/cache" \
    "$LEX" pkg install --trusted-keys "$WORK/keyring.json" )

# ── 3b. REFUSAL — a substituted archive fails the integrity gate ──────────────
say "3b. Refusal — a tampered archive is rejected (content-hash integrity)"
echo "not the published bytes" > "$WORK/registry/v1/pkg/weather/1.0.0/archive"
rm -rf "$WORK/cache"
if ( cd "$WORK/app" && LEX_PACKAGES_DIR="$WORK/cache" \
       "$LEX" pkg install --trusted-keys "$WORK/keyring.json" ) 2>"$WORK/err.txt"; then
  echo "UNEXPECTED: tampered archive installed" >&2; exit 1
fi
note "refused: $(grep -o 'integrity:.*' "$WORK/err.txt" | head -1)"
note "^ the signature bound the contract to the *original* bytes; substitution can't pass."
cp "$WORK/archive.tar" "$WORK/registry/v1/pkg/weather/1.0.0/archive"   # restore

# ── 4. ATTEST — promote an install record into the attestation graph ──────────
say "4. Attest — an install (recorded by lex-os) becomes durable evidence"
cat > "$WORK/install.audit.json" <<EOF
[ { "seq": 0, "prev_hash": "", "hash": "a",
    "event": { "kind": "capsule_installed", "artifact": "weather@1.0.0",
               "signer": "$PUBLIC",
               "content_hash": "$(python3 -c "import json;print(json.load(open('$WORK/contract.json'))['contract']['artifact']['content_hash'])")",
               "effective_grant": "fs=read-only net=allowlist exec=none" } } ]
EOF
note "(this audit log is what \`lex-os capsule install --audit-out\` writes)"
"$LEX" attest import-install --audit "$WORK/install.audit.json" --store "$STORE"
"$LEX" --output json attest filter --kind capsule_install --store "$STORE" \
  | python3 -c "import sys,json;d=json.load(sys.stdin)['data'];print('    queryable:',d['count'],'capsule_install attestation(s), keyed under the signer')"

# ── 5. EARNED TRUST — the keyring the NEXT install will pin ───────────────────
say "5. Earned trust — the install becomes track record that gates the next one"
"$LEX" --output json producer-trust recompute --tool "$PUBLIC" --store "$STORE" >/dev/null
note "trusted-keys keyring earned from track record (min-trust 700):"
"$LEX" producer-trust keyring --store "$STORE" --min-trust 700

say "Done — derive-grant → sign → verify-on-install → attest → earned trust, one loop."
note "The same contract format and keyring drive \`lex-os capsule install\` (see lex-os)."
