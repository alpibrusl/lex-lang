#!/usr/bin/env bash
# Examples for: op

# List ops
lex op log

# Replay-verify an op
lex op replay <op_id> --candidate regen.lex

# Garbage-collect
lex op gc --confirm
