#!/usr/bin/env bash
# Examples for: export-git

# Export main to ./export
lex export-git ./export

# Export a feature branch
lex export-git ./out --branch feature

# Append new ops to an existing export
lex export-git ./export --incremental
