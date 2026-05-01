#!/usr/bin/env bash
# Examples for: replay

# Replay verbatim
lex replay 2024-01-15-abc app.lex main

# Replay with override
lex replay 2024-01-15-abc app.lex main --override 7=42
