#!/usr/bin/env bash
# Examples for: producer-trust

# Recompute a tool's trust
lex producer-trust recompute --tool <id>

# Export a capsule keyring
lex producer-trust keyring --min-trust 700 --out trusted.json
