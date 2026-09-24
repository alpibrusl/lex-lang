#!/usr/bin/env bash
# Examples for: files

# What changed since the last publish
lex files status

# Record a files-only change
lex files commit -m 'update README'

# List the head's manifest
lex files ls

# Print a manifest file
lex files cat README.md

# Materialize a package without git
lex files checkout /tmp/out
