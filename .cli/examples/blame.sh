#!/usr/bin/env bash
# Examples for: blame

# Blame a file
lex blame app.lex

# With evidence trail
lex blame --with-evidence app.lex

# Machine-readable
lex --output json blame app.lex
