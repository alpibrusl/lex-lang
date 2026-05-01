#!/usr/bin/env bash
# Examples for: audit

# Find network calls
lex audit --effect net examples

# Find a host as JSON
lex audit --host api.example.com --json src
