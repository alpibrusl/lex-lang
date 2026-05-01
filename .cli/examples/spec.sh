#!/usr/bin/env bash
# Examples for: spec

# Check a spec
lex spec check sort.spec --source sort.lex

# Export SMT-LIB
lex spec smt sort.spec
