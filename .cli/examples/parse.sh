#!/usr/bin/env bash
# Examples for: parse

# Parse a file
lex parse hello.lex

# Parse stdin
cat hello.lex | lex parse -
