#!/usr/bin/env bash
# Examples for: pkg

# Install deps
lex pkg install

# Install, verifying each registry dep's signed contract against pinned publishers
lex pkg install --trusted-keys keyring.json --require-contracts

# Add a path dep
lex pkg add mylib --path ../mylib

# Publish with a contract whose grant is derived from the code's effects
lex pkg publish --sign <key> --derive-grant --contract-out c.json --archive-out pkg.tar

# Verify a package against its contract
lex pkg verify --archive pkg.tar --contract c.json --trusted-keys keyring.json
