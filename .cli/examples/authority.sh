#!/usr/bin/env bash
# Examples for: authority

# What authority does this package need?
lex authority derive src/

# Refuse a change that reaches somewhere new
lex authority diff --base old/ --head src/ --fail-on widening
