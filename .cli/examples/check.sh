#!/usr/bin/env bash
# Examples for: check

# Type-check a file
lex check hello.lex

# Check before running
lex check app.lex && lex run app.lex main
