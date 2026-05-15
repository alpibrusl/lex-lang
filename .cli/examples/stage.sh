#!/usr/bin/env bash
# Examples for: stage

# Print stage info
lex stage abc123...

# List attestations
lex stage abc123... --attestations

# Machine-readable
lex --output json stage abc123... --attestations
