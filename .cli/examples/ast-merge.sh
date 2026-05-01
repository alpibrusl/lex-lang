#!/usr/bin/env bash
# Examples for: ast-merge

# Merge three versions
lex ast-merge base.lex ours.lex theirs.lex

# Materialize merge
lex ast-merge base.lex ours.lex theirs.lex --output merged.lex
