#!/usr/bin/env bash
# Examples for: agent-tool

# Run with allow-list
lex agent-tool --allow-effects fs_read --request "sum lines of /tmp/log"

# Verify against spec
lex agent-tool --spec sort.spec --body-file body.lex --json
