#!/usr/bin/env bash
# Examples for: branch

# List branches
lex branch list

# Create a feature
lex branch create feature --from main

# Peek what feature has done since fork
lex branch peek feature --since-fork

# Preview merging feature into main
lex branch overlay feature --on main
