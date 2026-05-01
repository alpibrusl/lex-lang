#!/usr/bin/env bash
# Examples for: hash

# Hash a file
lex hash app.lex

# Diff hashes across versions
diff <(lex hash a.lex) <(lex hash b.lex)
