#!/usr/bin/env bash
# Examples for: issue

# Declare a feature as a typed delta (API entry + example)
lex issue create --title "add gcd" --shape typed_delta --api "gcd:(a :: Int, b :: Int) -> Int" --example "gcd(12, 8) => 4"

# File a bug as a failing example (fixed = it passes)
lex issue create --title "gcd(0,0) crashes" --shape failing_example --example "gcd(0, 0) => 0"

# A metric target (ops/growth): a predicate over the event backbone
lex issue create --title "p99 under 200ms" --shape metric_invariant --predicate "p99 < 200" --window 7d

# List issues
lex issue list

# Show one as JSON
lex issue show <id>

# Verify an issue at the branch head — done is a proof the gate records, not a status
lex issue verify <id>
