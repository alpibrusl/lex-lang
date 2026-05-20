# IC polymorphism measurement (#462 slice 2b go/no-go)

**Date:** 2026-05-18
**Branch:** `claude/issue-462-ic-shape-stats` (throwaway, do not merge to main)
**Question:** Before implementing slice 2b (polymorphic IC keyed on
`Value::Record.shape_id`), measure how often real Lex programs hit
sites that see more than one record shape. If polymorphism is rare,
2b is theatre and we should pivot to the dispatch rewrite (#461 next
phase).

## Method

Added an env-gated instrumentation hook to `Op::GetField` in
`crates/lex-bytecode/src/vm.rs`. With `LEX_IC_STATS=1` set, every
field access records `(fn_id, site_idx) -> shape_id -> hit_count`
into a process-global map. On `Vm::drop`, dumps a TSV to
`$LEX_IC_STATS_OUT.$PID` (or stderr).

Ran six representative test binaries under `cargo test -- --test-threads=1`:

| binary | what it exercises |
|---|---|
| `inbox_app` | full mailbox app — auth, list ops, record builders |
| `gateway_app` | HTTP gateway with routing/middleware |
| `analytics_app` | numeric aggregation over typed records |
| `ml_app` | ML pipeline with feature records |
| `std_http` | http.send + request/response record handling |
| `list_sort_by` | record-field comparator paths |

## Results

```
workload        sites   mono   poly  hits  real_shape  NO_SHAPE
inbox_app          10     10      0    23           0        23
gateway_app         6      6      0    13           0        13
analytics_app      11     11      0   339         308        31
ml_app             10     10      0   318         300        18
std_http            5      5      0     7           0         7
list_sort_by        1      1      0     8           0         8
─────────────────────────────────────────────────────────────────
TOTAL              43     43      0   708         608       100

Polymorphic site rate: 0.0%
Polymorphic hit rate:  0.0%
NO_SHAPE_ID share:    14.1% of hits, 100% of inbox/gateway/std_http/list_sort_by traffic
```

## Findings

1. **Polymorphism rate is 0%.** Not "below threshold" — exactly zero.
   Of 43 distinct `(fn_id, site_idx)` pairs across all measured runs,
   every single one observed exactly one `shape_id`. Slice 2b would
   add a 4-way shape dispatch on top of the existing monomorphic IC
   for sites that will never have more than one shape.

2. **The type system explains it.** Lex's checker enforces a single
   concrete record type per call site. Polymorphism could only show
   up via subtyping/union refinement at a record field consumer —
   which our test corpus does not exercise. Need real workloads with
   `Variant`-typed record consumers to ever see >1 shape.

3. **NO_SHAPE_ID is a much bigger problem than polymorphism.** 14% of
   measured hits (and 100% of HTTP/inbox/gateway traffic) land on
   records constructed via the dynamic path (`Value::record_dynamic`,
   sentinel `u32::MAX`). These records share one bucket and would
   collide under any shape-keyed IC. Slice 3 (propagate shape_ids
   through JSON decode, SQL row, HTTP header, builtin returns) is a
   prerequisite for any shape-keyed dispatch to be useful at all.

## Decision

**Skip slice 2b.** Two reasons:

- Zero observed polymorphism → 2b has zero upside on this corpus.
- The 14% of hits that *would* break a shape-keyed IC (NO_SHAPE_ID)
  are not yet addressed by slice 3.

**Pivot to dispatch rewrite (#461 next phase).** Superinstructions
already captured 64% of the addressable dispatch overhead (+27% on
`straight_arith`). The remaining ~36% lives in the match-arm
interpreter dispatch loop; a function-table / computed-goto rewrite
is the next lever. That's a known win with no preconditions, vs.
slice 2b which has zero measured upside.

If we later see real workloads (production traces from the
inbox/gateway apps, lex-search heavy queries with mixed result
shapes) that show polymorphism >5%, revisit slice 2b — but only
after slice 3 lands so NO_SHAPE_ID records carry real shapes.

## Reproducing

```sh
git checkout claude/issue-462-ic-shape-stats
cargo build -p lex-runtime --test inbox_app --test gateway_app \
  --test analytics_app --test ml_app --test std_http --test list_sort_by

mkdir -p /tmp/icstats && rm -f /tmp/icstats/*
for t in inbox_app gateway_app analytics_app ml_app std_http list_sort_by; do
  LEX_IC_STATS=1 LEX_IC_STATS_OUT=/tmp/icstats/run \
    cargo test -p lex-runtime --test $t -- --test-threads=1 >/dev/null 2>&1
done
# Each /tmp/icstats/run.<pid> is the data for one test binary.
```
