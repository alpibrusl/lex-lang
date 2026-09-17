#!/usr/bin/env bash
# Examples for: propagate

# Rename a symbol across every dependent in a workspace
lex propagate --package lex-nt --rename gcd=euclidean_gcd --workspace . --apply

# Agent-migrate dependents after a behavioral change (gated on type-check)
lex propagate --package lex-nt --symbol euler_phi --note 'handles 0 now' --ollama
