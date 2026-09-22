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

# Refine a free-form issue: an agent proposes a typed acceptance (#956)
lex issue propose <id> --shape typed_delta --api "clamp:(x :: Int, lo :: Int, hi :: Int) -> Int" --example "clamp(5, 0, 3) => 3" --rationale "inclusive bounds" --by lex-code

# A human approves it — the issue is judged against it from then on
lex issue approve <proposal> --by alfonso
