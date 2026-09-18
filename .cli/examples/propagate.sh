#!/usr/bin/env bash
# Examples for: propagate

# Rename a symbol across every dependent in a workspace
lex propagate --package lex-nt --rename gcd=euclidean_gcd --workspace . --apply

# Auto-detect renames from two hosted releases and fan them out
lex propagate --package lex-nt --from 1.4.0 --to 2.0.0 --registry vcs.lexlang.org/lex-official/lex-nt --workspace . --apply

# Agent-migrate dependents after a behavioral change (gated on type-check)
lex propagate --package lex-nt --symbol euler_phi --note 'handles 0 now' --ollama
