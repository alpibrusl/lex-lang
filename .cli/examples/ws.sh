#!/usr/bin/env bash
# Examples for: ws

# Inline a let binding
lex ws transform --branch main inline_let --json '{"from_stage_id":"<stage>","let_node":"n_0.2"}'
