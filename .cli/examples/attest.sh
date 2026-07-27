#!/usr/bin/env bash
# Examples for: attest

# All Spec verdicts that passed
lex attest filter --kind spec --result passed

# Recent type-check evidence
lex attest filter --kind type_check --since 2026-05-01

# Promote capsule installs to attestations
lex attest import-install --audit install.audit.json

# Machine-readable
lex --output json attest filter --kind spec
