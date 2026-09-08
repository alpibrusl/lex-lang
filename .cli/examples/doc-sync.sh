#!/usr/bin/env bash
# Examples for: doc-sync

# Regenerate all targets
lex doc-sync

# CI drift gate
lex doc-sync --check
